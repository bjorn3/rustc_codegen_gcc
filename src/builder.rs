use std::borrow::Cow;
use std::cell::Cell;
use std::convert::TryFrom;
use std::ops::Deref;

use gccjit::{
    BinaryOp,
    Block,
    ComparisonOp,
    Function,
    LValue,
    RValue,
    ToRValue,
    Type,
    UnaryOp,
};
use rustc_codegen_ssa::MemFlags;
use rustc_codegen_ssa::common::{AtomicOrdering, AtomicRmwBinOp, IntPredicate, RealPredicate, SynchronizationScope};
use rustc_codegen_ssa::mir::operand::{OperandRef, OperandValue};
use rustc_codegen_ssa::mir::place::PlaceRef;
use rustc_codegen_ssa::traits::{
    BackendTypes,
    BaseTypeMethods,
    BuilderMethods,
    ConstMethods,
    DerivedTypeMethods,
    LayoutTypeMethods,
    HasCodegen,
    OverflowOp,
    StaticBuilderMethods,
};
use rustc_data_structures::stable_set::FxHashSet;
use rustc_middle::ty::{ParamEnv, Ty, TyCtxt};
use rustc_middle::ty::layout::{FnAbiError, FnAbiOfHelpers, FnAbiRequest, HasParamEnv, HasTyCtxt, LayoutError, LayoutOfHelpers, TyAndLayout};
use rustc_span::Span;
use rustc_span::def_id::DefId;
use rustc_target::abi::{
    self,
    call::FnAbi,
    Align,
    HasDataLayout,
    Size,
    TargetDataLayout,
    WrappingRange,
};
use rustc_target::spec::{HasTargetSpec, Target};

use crate::common::{SignType, TypeReflection, type_is_pointer};
use crate::context::CodegenCx;
use crate::type_of::LayoutGccExt;

// TODO(antoyo)
type Funclet = ();

// TODO(antoyo): remove this variable.
static mut RETURN_VALUE_COUNT: usize = 0;

enum ExtremumOperation {
    Max,
    Min,
}

trait EnumClone {
    fn clone(&self) -> Self;
}

impl EnumClone for AtomicOrdering {
    fn clone(&self) -> Self {
        match *self {
            AtomicOrdering::NotAtomic => AtomicOrdering::NotAtomic,
            AtomicOrdering::Unordered => AtomicOrdering::Unordered,
            AtomicOrdering::Monotonic => AtomicOrdering::Monotonic,
            AtomicOrdering::Acquire => AtomicOrdering::Acquire,
            AtomicOrdering::Release => AtomicOrdering::Release,
            AtomicOrdering::AcquireRelease => AtomicOrdering::AcquireRelease,
            AtomicOrdering::SequentiallyConsistent => AtomicOrdering::SequentiallyConsistent,
        }
    }
}

pub struct Builder<'a: 'gcc, 'gcc, 'tcx> {
    pub cx: &'a CodegenCx<'gcc, 'tcx>,
    pub block: Block<'gcc>,
    stack_var_count: Cell<usize>,
}

impl<'a, 'gcc, 'tcx> Builder<'a, 'gcc, 'tcx> {
    fn with_cx(cx: &'a CodegenCx<'gcc, 'tcx>, block: Block<'gcc>) -> Self {
        Builder {
            cx,
            block,
            stack_var_count: Cell::new(0),
        }
    }

    fn atomic_extremum(&mut self, operation: ExtremumOperation, dst: RValue<'gcc>, src: RValue<'gcc>, order: AtomicOrdering) -> RValue<'gcc> {
        let size = src.get_type().get_size();

        let func = self.current_func();

        let load_ordering =
            match order {
                // TODO(antoyo): does this make sense?
                AtomicOrdering::AcquireRelease | AtomicOrdering::Release => AtomicOrdering::Acquire,
                _ => order.clone(),
            };
        let previous_value = self.atomic_load(dst.get_type(), dst, load_ordering.clone(), Size::from_bytes(size));
        let previous_var = func.new_local(None, previous_value.get_type(), "previous_value");
        let return_value = func.new_local(None, previous_value.get_type(), "return_value");
        self.llbb().add_assignment(None, previous_var, previous_value);
        self.llbb().add_assignment(None, return_value, previous_var.to_rvalue());

        let while_block = func.new_block("while");
        let after_block = func.new_block("after_while");
        self.llbb().end_with_jump(None, while_block);

        // NOTE: since jumps were added and compare_exchange doesn't expect this, the current block in the
        // state need to be updated.
        self.switch_to_block(while_block);

        let comparison_operator =
            match operation {
                ExtremumOperation::Max => ComparisonOp::LessThan,
                ExtremumOperation::Min => ComparisonOp::GreaterThan,
            };

        let cond1 = self.context.new_comparison(None, comparison_operator, previous_var.to_rvalue(), self.context.new_cast(None, src, previous_value.get_type()));
        let compare_exchange = self.compare_exchange(dst, previous_var, src, order, load_ordering, false);
        let cond2 = self.cx.context.new_unary_op(None, UnaryOp::LogicalNegate, compare_exchange.get_type(), compare_exchange);
        let cond = self.cx.context.new_binary_op(None, BinaryOp::LogicalAnd, self.cx.bool_type, cond1, cond2);

        while_block.end_with_conditional(None, cond, while_block, after_block);

        // NOTE: since jumps were added in a place rustc does not expect, the current block in the
        // state need to be updated.
        self.switch_to_block(after_block);

        return_value.to_rvalue()
    }

    fn compare_exchange(&self, dst: RValue<'gcc>, cmp: LValue<'gcc>, src: RValue<'gcc>, order: AtomicOrdering, failure_order: AtomicOrdering, weak: bool) -> RValue<'gcc> {
        let size = src.get_type().get_size();
        let compare_exchange = self.context.get_builtin_function(&format!("__atomic_compare_exchange_{}", size));
        let order = self.context.new_rvalue_from_int(self.i32_type, order.to_gcc());
        let failure_order = self.context.new_rvalue_from_int(self.i32_type, failure_order.to_gcc());
        let weak = self.context.new_rvalue_from_int(self.bool_type, weak as i32);

        let void_ptr_type = self.context.new_type::<*mut ()>();
        let volatile_void_ptr_type = void_ptr_type.make_volatile();
        let dst = self.context.new_cast(None, dst, volatile_void_ptr_type);
        let expected = self.context.new_cast(None, cmp.get_address(None), void_ptr_type);

        // NOTE: not sure why, but we have the wrong type here.
        let int_type = compare_exchange.get_param(2).to_rvalue().get_type();
        let src = self.context.new_cast(None, src, int_type);
        self.context.new_call(None, compare_exchange, &[dst, expected, src, weak, order, failure_order])
    }

    pub fn assign(&self, lvalue: LValue<'gcc>, value: RValue<'gcc>) {
        self.llbb().add_assignment(None, lvalue, value);
    }

    fn check_call<'b>(&mut self, _typ: &str, func: Function<'gcc>, args: &'b [RValue<'gcc>]) -> Cow<'b, [RValue<'gcc>]> {
        let mut all_args_match = true;
        let mut param_types = vec![];
        let param_count = func.get_param_count();
        for (index, arg) in args.iter().enumerate().take(param_count) {
            let param = func.get_param(index as i32);
            let param = param.to_rvalue().get_type();
            if param != arg.get_type() {
                all_args_match = false;
            }
            param_types.push(param);
        }

        if all_args_match {
            return Cow::Borrowed(args);
        }

        let casted_args: Vec<_> = param_types
            .into_iter()
            .zip(args.iter())
            .enumerate()
            .map(|(_i, (expected_ty, &actual_val))| {
                let actual_ty = actual_val.get_type();
                if expected_ty != actual_ty {
                    self.bitcast(actual_val, expected_ty)
                }
                else {
                    actual_val
                }
            })
            .collect();

        Cow::Owned(casted_args)
    }

    fn check_ptr_call<'b>(&mut self, _typ: &str, func_ptr: RValue<'gcc>, args: &'b [RValue<'gcc>]) -> Cow<'b, [RValue<'gcc>]> {
        let mut all_args_match = true;
        let mut param_types = vec![];
        let gcc_func = func_ptr.get_type().dyncast_function_ptr_type().expect("function ptr");
        for (index, arg) in args.iter().enumerate().take(gcc_func.get_param_count()) {
            let param = gcc_func.get_param_type(index);
            if param != arg.get_type() {
                all_args_match = false;
            }
            param_types.push(param);
        }

        let mut on_stack_param_indices = FxHashSet::default();
        if let Some(indices) = self.on_stack_params.borrow().get(&gcc_func) {
            on_stack_param_indices = indices.clone();
        }

        if all_args_match {
            return Cow::Borrowed(args);
        }

        let casted_args: Vec<_> = param_types
            .into_iter()
            .zip(args.iter())
            .enumerate()
            .map(|(index, (expected_ty, &actual_val))| {
                let actual_ty = actual_val.get_type();
                if expected_ty != actual_ty {
                    if !actual_ty.is_vector() && !expected_ty.is_vector() && actual_ty.is_integral() && expected_ty.is_integral() && actual_ty.get_size() != expected_ty.get_size() {
                        self.context.new_cast(None, actual_val, expected_ty)
                    }
                    else if on_stack_param_indices.contains(&index) {
                        actual_val.dereference(None).to_rvalue()
                    }
                    else {
                        assert!(!((actual_ty.is_vector() && !expected_ty.is_vector()) || (!actual_ty.is_vector() && expected_ty.is_vector())), "{:?} ({}) -> {:?} ({}), index: {:?}[{}]", actual_ty, actual_ty.is_vector(), expected_ty, expected_ty.is_vector(), func_ptr, index);
                        // TODO(antoyo): perhaps use __builtin_convertvector for vector casting.
                        self.bitcast(actual_val, expected_ty)
                    }
                }
                else {
                    actual_val
                }
            })
            .collect();

        Cow::Owned(casted_args)
    }

    fn check_store(&mut self, val: RValue<'gcc>, ptr: RValue<'gcc>) -> RValue<'gcc> {
        let dest_ptr_ty = self.cx.val_ty(ptr).make_pointer(); // TODO(antoyo): make sure make_pointer() is okay here.
        let stored_ty = self.cx.val_ty(val);
        let stored_ptr_ty = self.cx.type_ptr_to(stored_ty);

        if dest_ptr_ty == stored_ptr_ty {
            ptr
        }
        else {
            self.bitcast(ptr, stored_ptr_ty)
        }
    }

    pub fn current_func(&self) -> Function<'gcc> {
        self.block.get_function()
    }

    fn function_call(&mut self, func: RValue<'gcc>, args: &[RValue<'gcc>], _funclet: Option<&Funclet>) -> RValue<'gcc> {
        // TODO(antoyo): remove when the API supports a different type for functions.
        let func: Function<'gcc> = self.cx.rvalue_as_function(func);
        let args = self.check_call("call", func, args);

        // gccjit requires to use the result of functions, even when it's not used.
        // That's why we assign the result to a local or call add_eval().
        let return_type = func.get_return_type();
        let void_type = self.context.new_type::<()>();
        let current_func = self.block.get_function();
        if return_type != void_type {
            unsafe { RETURN_VALUE_COUNT += 1 };
            let result = current_func.new_local(None, return_type, &format!("returnValue{}", unsafe { RETURN_VALUE_COUNT }));
            self.block.add_assignment(None, result, self.cx.context.new_call(None, func, &args));
            result.to_rvalue()
        }
        else {
            self.block.add_eval(None, self.cx.context.new_call(None, func, &args));
            // Return dummy value when not having return value.
            self.context.new_rvalue_from_long(self.isize_type, 0)
        }
    }

    fn function_ptr_call(&mut self, func_ptr: RValue<'gcc>, args: &[RValue<'gcc>], _funclet: Option<&Funclet>) -> RValue<'gcc> {
        let args = self.check_ptr_call("call", func_ptr, args);

        // gccjit requires to use the result of functions, even when it's not used.
        // That's why we assign the result to a local or call add_eval().
        let gcc_func = func_ptr.get_type().dyncast_function_ptr_type().expect("function ptr");
        let return_type = gcc_func.get_return_type();
        let void_type = self.context.new_type::<()>();
        let current_func = self.block.get_function();

        if return_type != void_type {
            unsafe { RETURN_VALUE_COUNT += 1 };
            let result = current_func.new_local(None, return_type, &format!("ptrReturnValue{}", unsafe { RETURN_VALUE_COUNT }));
            self.block.add_assignment(None, result, self.cx.context.new_call_through_ptr(None, func_ptr, &args));
            result.to_rvalue()
        }
        else {
            self.block.add_eval(None, self.cx.context.new_call_through_ptr(None, func_ptr, &args));
            // Return dummy value when not having return value.
            let result = current_func.new_local(None, self.isize_type, "dummyValueThatShouldNeverBeUsed");
            self.block.add_assignment(None, result, self.context.new_rvalue_from_long(self.isize_type, 0));
            result.to_rvalue()
        }
    }

    pub fn overflow_call(&self, func: Function<'gcc>, args: &[RValue<'gcc>], _funclet: Option<&Funclet>) -> RValue<'gcc> {
        // gccjit requires to use the result of functions, even when it's not used.
        // That's why we assign the result to a local.
        let return_type = self.context.new_type::<bool>();
        let current_func = self.block.get_function();
        // TODO(antoyo): return the new_call() directly? Since the overflow function has no side-effects.
        unsafe { RETURN_VALUE_COUNT += 1 };
        let result = current_func.new_local(None, return_type, &format!("overflowReturnValue{}", unsafe { RETURN_VALUE_COUNT }));
        self.block.add_assignment(None, result, self.cx.context.new_call(None, func, &args));
        result.to_rvalue()
    }
}

impl<'gcc, 'tcx> HasCodegen<'tcx> for Builder<'_, 'gcc, 'tcx> {
    type CodegenCx = CodegenCx<'gcc, 'tcx>;
}

impl<'tcx> HasTyCtxt<'tcx> for Builder<'_, '_, 'tcx> {
    fn tcx(&self) -> TyCtxt<'tcx> {
        self.cx.tcx()
    }
}

impl HasDataLayout for Builder<'_, '_, '_> {
    fn data_layout(&self) -> &TargetDataLayout {
        self.cx.data_layout()
    }
}

impl<'tcx> LayoutOfHelpers<'tcx> for Builder<'_, '_, 'tcx> {
    type LayoutOfResult = TyAndLayout<'tcx>;

    #[inline]
    fn handle_layout_err(&self, err: LayoutError<'tcx>, span: Span, ty: Ty<'tcx>) -> ! {
        self.cx.handle_layout_err(err, span, ty)
    }
}

impl<'tcx> FnAbiOfHelpers<'tcx> for Builder<'_, '_, 'tcx> {
    type FnAbiOfResult = &'tcx FnAbi<'tcx, Ty<'tcx>>;

    #[inline]
    fn handle_fn_abi_err(
        &self,
        err: FnAbiError<'tcx>,
        span: Span,
        fn_abi_request: FnAbiRequest<'tcx>,
    ) -> ! {
        self.cx.handle_fn_abi_err(err, span, fn_abi_request)
    }
}

impl<'gcc, 'tcx> Deref for Builder<'_, 'gcc, 'tcx> {
    type Target = CodegenCx<'gcc, 'tcx>;

    fn deref(&self) -> &Self::Target {
        self.cx
    }
}

impl<'gcc, 'tcx> BackendTypes for Builder<'_, 'gcc, 'tcx> {
    type Value = <CodegenCx<'gcc, 'tcx> as BackendTypes>::Value;
    type Function = <CodegenCx<'gcc, 'tcx> as BackendTypes>::Function;
    type BasicBlock = <CodegenCx<'gcc, 'tcx> as BackendTypes>::BasicBlock;
    type Type = <CodegenCx<'gcc, 'tcx> as BackendTypes>::Type;
    type Funclet = <CodegenCx<'gcc, 'tcx> as BackendTypes>::Funclet;

    type DIScope = <CodegenCx<'gcc, 'tcx> as BackendTypes>::DIScope;
    type DILocation = <CodegenCx<'gcc, 'tcx> as BackendTypes>::DILocation;
    type DIVariable = <CodegenCx<'gcc, 'tcx> as BackendTypes>::DIVariable;
}

impl<'a, 'gcc, 'tcx> BuilderMethods<'a, 'tcx> for Builder<'a, 'gcc, 'tcx> {
    fn build(cx: &'a CodegenCx<'gcc, 'tcx>, block: Block<'gcc>) -> Self {
        Builder::with_cx(cx, block)
    }

    fn llbb(&self) -> Block<'gcc> {
        self.block
    }

    fn append_block(cx: &'a CodegenCx<'gcc, 'tcx>, func: RValue<'gcc>, name: &str) -> Block<'gcc> {
        let func = cx.rvalue_as_function(func);
        func.new_block(name)
    }

    fn append_sibling_block(&mut self, name: &str) -> Block<'gcc> {
        let func = self.current_func();
        func.new_block(name)
    }

    fn switch_to_block(&mut self, block: Self::BasicBlock) {
        self.block = block;
    }

    fn ret_void(&mut self) {
        self.llbb().end_with_void_return(None)
    }

    fn ret(&mut self, value: RValue<'gcc>) {
        let value =
            if self.structs_as_pointer.borrow().contains(&value) {
                // NOTE: hack to workaround a limitation of the rustc API: see comment on
                // CodegenCx.structs_as_pointer
                value.dereference(None).to_rvalue()
            }
            else {
                value
            };
        self.llbb().end_with_return(None, value);
    }

    fn br(&mut self, dest: Block<'gcc>) {
        self.llbb().end_with_jump(None, dest)
    }

    fn cond_br(&mut self, cond: RValue<'gcc>, then_block: Block<'gcc>, else_block: Block<'gcc>) {
        self.llbb().end_with_conditional(None, cond, then_block, else_block)
    }

    fn switch(&mut self, value: RValue<'gcc>, default_block: Block<'gcc>, cases: impl ExactSizeIterator<Item = (u128, Block<'gcc>)>) {
        let mut gcc_cases = vec![];
        let typ = self.val_ty(value);
        for (on_val, dest) in cases {
            let on_val = self.const_uint_big(typ, on_val);
            gcc_cases.push(self.context.new_case(on_val, on_val, dest));
        }
        self.block.end_with_switch(None, value, default_block, &gcc_cases);
    }

    fn invoke(&mut self, typ: Type<'gcc>, func: RValue<'gcc>, args: &[RValue<'gcc>], then: Block<'gcc>, catch: Block<'gcc>, _funclet: Option<&Funclet>) -> RValue<'gcc> {
        // TODO(bjorn3): Properly implement unwinding.
        let call_site = self.call(typ, func, args, None);
        let condition = self.context.new_rvalue_from_int(self.bool_type, 1);
        self.llbb().end_with_conditional(None, condition, then, catch);
        call_site
    }

    fn unreachable(&mut self) {
        let func = self.context.get_builtin_function("__builtin_unreachable");
        self.block.add_eval(None, self.context.new_call(None, func, &[]));
        let return_type = self.block.get_function().get_return_type();
        let void_type = self.context.new_type::<()>();
        if return_type == void_type {
            self.block.end_with_void_return(None)
        }
        else {
            let return_value = self.current_func()
                .new_local(None, return_type, "unreachableReturn");
            self.block.end_with_return(None, return_value)
        }
    }

    fn add(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        self.gcc_add(a, b)
    }

    fn fadd(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        a + b
    }

    fn sub(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        self.gcc_sub(a, b)
    }

    fn fsub(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        a - b
    }

    fn mul(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        self.gcc_mul(a, b)
    }

    fn fmul(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        a * b
    }

    fn udiv(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        self.gcc_udiv(a, b)
    }

    fn exactudiv(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        // TODO(antoyo): convert the arguments to unsigned?
        // TODO(antoyo): poison if not exact.
        a / b
    }

    fn sdiv(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        self.gcc_sdiv(a, b)
    }

    fn exactsdiv(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        // TODO(antoyo): posion if not exact.
        // FIXME(antoyo): rustc_codegen_ssa::mir::intrinsic uses different types for a and b but they
        // should be the same.
        let typ = a.get_type().to_signed(self);
        let b = self.context.new_cast(None, b, typ);
        a / b
    }

    fn fdiv(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        a / b
    }

    fn urem(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        self.gcc_urem(a, b)
    }

    fn srem(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        self.gcc_srem(a, b)
    }

    fn frem(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        if a.get_type().is_compatible_with(self.cx.float_type) {
            let fmodf = self.context.get_builtin_function("fmodf");
            // FIXME(antoyo): this seems to produce the wrong result.
            return self.context.new_call(None, fmodf, &[a, b]);
        }
        assert_eq!(a.get_type().unqualified(), self.cx.double_type);

        let fmod = self.context.get_builtin_function("fmod");
        return self.context.new_call(None, fmod, &[a, b]);
    }

    fn shl(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        self.gcc_shl(a, b)
    }

    fn lshr(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        self.gcc_lshr(a, b)
    }

    fn ashr(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        // TODO(antoyo): check whether behavior is an arithmetic shift for >> .
        // It seems to be if the value is signed.
        self.gcc_lshr(a, b)
    }

    fn and(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        self.gcc_and(a, b)
    }

    fn or(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        self.cx.gcc_or(a, b)
    }

    fn xor(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        self.gcc_xor(a, b)
    }

    fn neg(&mut self, a: RValue<'gcc>) -> RValue<'gcc> {
        self.gcc_neg(a)
    }

    fn fneg(&mut self, a: RValue<'gcc>) -> RValue<'gcc> {
        self.cx.context.new_unary_op(None, UnaryOp::Minus, a.get_type(), a)
    }

    fn not(&mut self, a: RValue<'gcc>) -> RValue<'gcc> {
        self.gcc_not(a)
    }

    fn unchecked_sadd(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        a + b
    }

    fn unchecked_uadd(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        self.gcc_add(a, b)
    }

    fn unchecked_ssub(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        a - b
    }

    fn unchecked_usub(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        // TODO(antoyo): should generate poison value?
        self.gcc_sub(a, b)
    }

    fn unchecked_smul(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        a * b
    }

    fn unchecked_umul(&mut self, a: RValue<'gcc>, b: RValue<'gcc>) -> RValue<'gcc> {
        a * b
    }

    fn fadd_fast(&mut self, _lhs: RValue<'gcc>, _rhs: RValue<'gcc>) -> RValue<'gcc> {
        unimplemented!();
    }

    fn fsub_fast(&mut self, _lhs: RValue<'gcc>, _rhs: RValue<'gcc>) -> RValue<'gcc> {
        unimplemented!();
    }

    fn fmul_fast(&mut self, _lhs: RValue<'gcc>, _rhs: RValue<'gcc>) -> RValue<'gcc> {
        unimplemented!();
    }

    fn fdiv_fast(&mut self, _lhs: RValue<'gcc>, _rhs: RValue<'gcc>) -> RValue<'gcc> {
        unimplemented!();
    }

    fn frem_fast(&mut self, _lhs: RValue<'gcc>, _rhs: RValue<'gcc>) -> RValue<'gcc> {
        unimplemented!();
    }

    fn checked_binop(&mut self, oop: OverflowOp, typ: Ty<'_>, lhs: Self::Value, rhs: Self::Value) -> (Self::Value, Self::Value) {
        self.gcc_checked_binop(oop, typ, lhs, rhs)
    }

    fn alloca(&mut self, ty: Type<'gcc>, align: Align) -> RValue<'gcc> {
        // FIXME(antoyo): this check that we don't call get_aligned() a second time on a type.
        // Ideally, we shouldn't need to do this check.
        let aligned_type =
            if ty == self.cx.u128_type || ty == self.cx.i128_type {
                ty
            }
            else {
                ty.get_aligned(align.bytes())
            };
        // TODO(antoyo): It might be better to return a LValue, but fixing the rustc API is non-trivial.
        self.stack_var_count.set(self.stack_var_count.get() + 1);
        self.current_func().new_local(None, aligned_type, &format!("stack_var_{}", self.stack_var_count.get())).get_address(None)
    }

    fn dynamic_alloca(&mut self, _ty: Type<'gcc>, _align: Align) -> RValue<'gcc> {
        unimplemented!();
    }

    fn array_alloca(&mut self, _ty: Type<'gcc>, _len: RValue<'gcc>, _align: Align) -> RValue<'gcc> {
        unimplemented!();
    }

    fn load(&mut self, _ty: Type<'gcc>, ptr: RValue<'gcc>, _align: Align) -> RValue<'gcc> {
        // TODO(antoyo): use ty.
        let block = self.llbb();
        let function = block.get_function();
        // NOTE: instead of returning the dereference here, we have to assign it to a variable in
        // the current basic block. Otherwise, it could be used in another basic block, causing a
        // dereference after a drop, for instance.
        // TODO(antoyo): handle align of the load instruction.
        let deref = ptr.dereference(None).to_rvalue();
        let value_type = deref.get_type();
        unsafe { RETURN_VALUE_COUNT += 1 };
        let loaded_value = function.new_local(None, value_type, &format!("loadedValue{}", unsafe { RETURN_VALUE_COUNT }));
        block.add_assignment(None, loaded_value, deref);
        loaded_value.to_rvalue()
    }

    fn volatile_load(&mut self, _ty: Type<'gcc>, ptr: RValue<'gcc>) -> RValue<'gcc> {
        // TODO(antoyo): use ty.
        let ptr = self.context.new_cast(None, ptr, ptr.get_type().make_volatile());
        ptr.dereference(None).to_rvalue()
    }

    fn atomic_load(&mut self, _ty: Type<'gcc>, ptr: RValue<'gcc>, order: AtomicOrdering, size: Size) -> RValue<'gcc> {
        // TODO(antoyo): use ty.
        // TODO(antoyo): handle alignment.
        let atomic_load = self.context.get_builtin_function(&format!("__atomic_load_{}", size.bytes()));
        let ordering = self.context.new_rvalue_from_int(self.i32_type, order.to_gcc());

        let volatile_const_void_ptr_type = self.context.new_type::<()>()
            .make_const()
            .make_volatile()
            .make_pointer();
        let ptr = self.context.new_cast(None, ptr, volatile_const_void_ptr_type);
        self.context.new_call(None, atomic_load, &[ptr, ordering])
    }

    fn load_operand(&mut self, place: PlaceRef<'tcx, RValue<'gcc>>) -> OperandRef<'tcx, RValue<'gcc>> {
        assert_eq!(place.llextra.is_some(), place.layout.is_unsized());

        if place.layout.is_zst() {
            return OperandRef::new_zst(self, place.layout);
        }

        fn scalar_load_metadata<'a, 'gcc, 'tcx>(bx: &mut Builder<'a, 'gcc, 'tcx>, load: RValue<'gcc>, scalar: &abi::Scalar) {
            let vr = scalar.valid_range.clone();
            match scalar.value {
                abi::Int(..) => {
                    if !scalar.is_always_valid(bx) {
                        bx.range_metadata(load, scalar.valid_range);
                    }
                }
                abi::Pointer if vr.start < vr.end && !vr.contains(0) => {
                    bx.nonnull_metadata(load);
                }
                _ => {}
            }
        }

        let val =
            if let Some(llextra) = place.llextra {
                OperandValue::Ref(place.llval, Some(llextra), place.align)
            }
            else if place.layout.is_gcc_immediate() {
                let load = self.load(place.llval.get_type(), place.llval, place.align);
                if let abi::Abi::Scalar(ref scalar) = place.layout.abi {
                    scalar_load_metadata(self, load, scalar);
                }
                OperandValue::Immediate(self.to_immediate(load, place.layout))
            }
            else if let abi::Abi::ScalarPair(ref a, ref b) = place.layout.abi {
                let b_offset = a.value.size(self).align_to(b.value.align(self).abi);
                let pair_type = place.layout.gcc_type(self, false);

                let mut load = |i, scalar: &abi::Scalar, align| {
                    let llptr = self.struct_gep(pair_type, place.llval, i as u64);
                    let load = self.load(llptr.get_type(), llptr, align);
                    scalar_load_metadata(self, load, scalar);
                    if scalar.is_bool() { self.trunc(load, self.type_i1()) } else { load }
                };

                OperandValue::Pair(
                    load(0, a, place.align),
                    load(1, b, place.align.restrict_for_offset(b_offset)),
                )
            }
            else {
                OperandValue::Ref(place.llval, None, place.align)
            };

        OperandRef { val, layout: place.layout }
    }

    fn write_operand_repeatedly(mut self, cg_elem: OperandRef<'tcx, RValue<'gcc>>, count: u64, dest: PlaceRef<'tcx, RValue<'gcc>>) -> Self {
        let zero = self.const_usize(0);
        let count = self.const_usize(count);
        let start = dest.project_index(&mut self, zero).llval;
        let end = dest.project_index(&mut self, count).llval;

        let header_bb = self.append_sibling_block("repeat_loop_header");
        let body_bb = self.append_sibling_block("repeat_loop_body");
        let next_bb = self.append_sibling_block("repeat_loop_next");

        let ptr_type = start.get_type();
        let current = self.llbb().get_function().new_local(None, ptr_type, "loop_var");
        let current_val = current.to_rvalue();
        self.assign(current, start);

        self.br(header_bb);

        self.switch_to_block(header_bb);
        let keep_going = self.icmp(IntPredicate::IntNE, current_val, end);
        self.cond_br(keep_going, body_bb, next_bb);

        self.switch_to_block(body_bb);
        let align = dest.align.restrict_for_offset(dest.layout.field(self.cx(), 0).size);
        cg_elem.val.store(&mut self, PlaceRef::new_sized_aligned(current_val, cg_elem.layout, align));

        let next = self.inbounds_gep(self.backend_type(cg_elem.layout), current.to_rvalue(), &[self.const_usize(1)]);
        self.llbb().add_assignment(None, current, next);
        self.br(header_bb);

        self.switch_to_block(next_bb);
        self
    }

    fn range_metadata(&mut self, _load: RValue<'gcc>, _range: WrappingRange) {
        // TODO(antoyo)
    }

    fn nonnull_metadata(&mut self, _load: RValue<'gcc>) {
        // TODO(antoyo)
    }

    fn type_metadata(&mut self, _function: RValue<'gcc>, _typeid: String) {
        // Unsupported.
    }

    fn typeid_metadata(&mut self, _typeid: String) -> RValue<'gcc> {
        // Unsupported.
        self.context.new_rvalue_from_int(self.int_type, 0)
    }


    fn store(&mut self, val: RValue<'gcc>, ptr: RValue<'gcc>, align: Align) -> RValue<'gcc> {
        self.store_with_flags(val, ptr, align, MemFlags::empty())
    }

    fn store_with_flags(&mut self, val: RValue<'gcc>, ptr: RValue<'gcc>, align: Align, _flags: MemFlags) -> RValue<'gcc> {
        let ptr = self.check_store(val, ptr);
        let destination = ptr.dereference(None);
        // NOTE: libgccjit does not support specifying the alignment on the assignment, so we cast
        // to type so it gets the proper alignment.
        let destination_type = destination.to_rvalue().get_type().unqualified();
        let aligned_type = destination_type.get_aligned(align.bytes()).make_pointer();
        let aligned_destination = self.cx.context.new_bitcast(None, ptr, aligned_type);
        let aligned_destination = aligned_destination.dereference(None);
        self.llbb().add_assignment(None, aligned_destination, val);
        // TODO(antoyo): handle align and flags.
        // NOTE: dummy value here since it's never used. FIXME(antoyo): API should not return a value here?
        self.cx.context.new_rvalue_zero(self.type_i32())
    }

    fn atomic_store(&mut self, value: RValue<'gcc>, ptr: RValue<'gcc>, order: AtomicOrdering, size: Size) {
        // TODO(antoyo): handle alignment.
        let atomic_store = self.context.get_builtin_function(&format!("__atomic_store_{}", size.bytes()));
        let ordering = self.context.new_rvalue_from_int(self.i32_type, order.to_gcc());
        let volatile_const_void_ptr_type = self.context.new_type::<()>()
            .make_volatile()
            .make_pointer();
        let ptr = self.context.new_cast(None, ptr, volatile_const_void_ptr_type);

        // FIXME(antoyo): fix libgccjit to allow comparing an integer type with an aligned integer type because
        // the following cast is required to avoid this error:
        // gcc_jit_context_new_call: mismatching types for argument 2 of function "__atomic_store_4": assignment to param arg1 (type: int) from loadedValue3577 (type: unsigned int  __attribute__((aligned(4))))
        let int_type = atomic_store.get_param(1).to_rvalue().get_type();
        let value = self.context.new_cast(None, value, int_type);
        self.llbb()
            .add_eval(None, self.context.new_call(None, atomic_store, &[ptr, value, ordering]));
    }

    fn gep(&mut self, _typ: Type<'gcc>, ptr: RValue<'gcc>, indices: &[RValue<'gcc>]) -> RValue<'gcc> {
        let mut result = ptr;
        for index in indices {
            result = self.context.new_array_access(None, result, *index).get_address(None).to_rvalue();
        }
        result
    }

    fn inbounds_gep(&mut self, _typ: Type<'gcc>, ptr: RValue<'gcc>, indices: &[RValue<'gcc>]) -> RValue<'gcc> {
        // FIXME(antoyo): would be safer if doing the same thing (loop) as gep.
        // TODO(antoyo): specify inbounds somehow.
        match indices.len() {
            1 => {
                self.context.new_array_access(None, ptr, indices[0]).get_address(None)
            },
            2 => {
                let array = ptr.dereference(None); // TODO(antoyo): assert that first index is 0?
                self.context.new_array_access(None, array, indices[1]).get_address(None)
            },
            _ => unimplemented!(),
        }
    }

    fn struct_gep(&mut self, value_type: Type<'gcc>, ptr: RValue<'gcc>, idx: u64) -> RValue<'gcc> {
        // FIXME(antoyo): it would be better if the API only called this on struct, not on arrays.
        assert_eq!(idx as usize as u64, idx);
        let value = ptr.dereference(None).to_rvalue();

        if value_type.dyncast_array().is_some() {
            let index = self.context.new_rvalue_from_long(self.u64_type, i64::try_from(idx).expect("i64::try_from"));
            let element = self.context.new_array_access(None, value, index);
            element.get_address(None)
        }
        else if let Some(vector_type) = value_type.dyncast_vector() {
            let array_type = vector_type.get_element_type().make_pointer();
            let array = self.bitcast(ptr, array_type);
            let index = self.context.new_rvalue_from_long(self.u64_type, i64::try_from(idx).expect("i64::try_from"));
            let element = self.context.new_array_access(None, array, index);
            element.get_address(None)
        }
        else if let Some(struct_type) = value_type.is_struct() {
            ptr.dereference_field(None, struct_type.get_field(idx as i32)).get_address(None)
        }
        else {
            panic!("Unexpected type {:?}", value_type);
        }
    }

    /* Casts */
    fn trunc(&mut self, value: RValue<'gcc>, dest_ty: Type<'gcc>) -> RValue<'gcc> {
        // TODO(antoyo): check that it indeed truncate the value.
        self.gcc_int_cast(value, dest_ty)
    }

    fn sext(&mut self, value: RValue<'gcc>, dest_ty: Type<'gcc>) -> RValue<'gcc> {
        // TODO(antoyo): check that it indeed sign extend the value.
        if dest_ty.dyncast_vector().is_some() {
            // TODO(antoyo): nothing to do as it is only for LLVM?
            return value;
        }
        self.context.new_cast(None, value, dest_ty)
    }

    fn fptoui(&mut self, value: RValue<'gcc>, dest_ty: Type<'gcc>) -> RValue<'gcc> {
        self.gcc_float_to_uint_cast(value, dest_ty)
    }

    fn fptosi(&mut self, value: RValue<'gcc>, dest_ty: Type<'gcc>) -> RValue<'gcc> {
        self.gcc_float_to_int_cast(value, dest_ty)
    }

    fn uitofp(&mut self, value: RValue<'gcc>, dest_ty: Type<'gcc>) -> RValue<'gcc> {
        self.gcc_uint_to_float_cast(value, dest_ty)
    }

    fn sitofp(&mut self, value: RValue<'gcc>, dest_ty: Type<'gcc>) -> RValue<'gcc> {
        self.gcc_int_to_float_cast(value, dest_ty)
    }

    fn fptrunc(&mut self, value: RValue<'gcc>, dest_ty: Type<'gcc>) -> RValue<'gcc> {
        // TODO(antoyo): make sure it truncates.
        self.context.new_cast(None, value, dest_ty)
    }

    fn fpext(&mut self, value: RValue<'gcc>, dest_ty: Type<'gcc>) -> RValue<'gcc> {
        self.context.new_cast(None, value, dest_ty)
    }

    fn ptrtoint(&mut self, value: RValue<'gcc>, dest_ty: Type<'gcc>) -> RValue<'gcc> {
        let usize_value = self.cx.const_bitcast(value, self.cx.type_isize());
        self.intcast(usize_value, dest_ty, false)
    }

    fn inttoptr(&mut self, value: RValue<'gcc>, dest_ty: Type<'gcc>) -> RValue<'gcc> {
        let usize_value = self.intcast(value, self.cx.type_isize(), false);
        self.cx.const_bitcast(usize_value, dest_ty)
    }

    fn bitcast(&mut self, value: RValue<'gcc>, dest_ty: Type<'gcc>) -> RValue<'gcc> {
        self.cx.const_bitcast(value, dest_ty)
    }

    fn intcast(&mut self, value: RValue<'gcc>, dest_typ: Type<'gcc>, _is_signed: bool) -> RValue<'gcc> {
        // NOTE: is_signed is for value, not dest_typ.
        self.gcc_int_cast(value, dest_typ)
    }

    fn pointercast(&mut self, value: RValue<'gcc>, dest_ty: Type<'gcc>) -> RValue<'gcc> {
        let val_type = value.get_type();
        match (type_is_pointer(val_type), type_is_pointer(dest_ty)) {
            (false, true) => {
                // NOTE: Projecting a field of a pointer type will attempt a cast from a signed char to
                // a pointer, which is not supported by gccjit.
                return self.cx.context.new_cast(None, self.inttoptr(value, val_type.make_pointer()), dest_ty);
            },
            (false, false) => {
                // When they are not pointers, we want a transmute (or reinterpret_cast).
                self.bitcast(value, dest_ty)
            },
            (true, true) => self.cx.context.new_cast(None, value, dest_ty),
            (true, false) => unimplemented!(),
        }
    }

    /* Comparisons */
    fn icmp(&mut self, op: IntPredicate, lhs: RValue<'gcc>, rhs: RValue<'gcc>) -> RValue<'gcc> {
        self.gcc_icmp(op, lhs, rhs)
    }

    fn fcmp(&mut self, op: RealPredicate, lhs: RValue<'gcc>, rhs: RValue<'gcc>) -> RValue<'gcc> {
        self.context.new_comparison(None, op.to_gcc_comparison(), lhs, rhs)
    }

    /* Miscellaneous instructions */
    fn memcpy(&mut self, dst: RValue<'gcc>, _dst_align: Align, src: RValue<'gcc>, _src_align: Align, size: RValue<'gcc>, flags: MemFlags) {
        assert!(!flags.contains(MemFlags::NONTEMPORAL), "non-temporal memcpy not supported");
        let size = self.intcast(size, self.type_size_t(), false);
        let _is_volatile = flags.contains(MemFlags::VOLATILE);
        let dst = self.pointercast(dst, self.type_i8p());
        let src = self.pointercast(src, self.type_ptr_to(self.type_void()));
        let memcpy = self.context.get_builtin_function("memcpy");
        // TODO(antoyo): handle aligns and is_volatile.
        self.block.add_eval(None, self.context.new_call(None, memcpy, &[dst, src, size]));
    }

    fn memmove(&mut self, dst: RValue<'gcc>, dst_align: Align, src: RValue<'gcc>, src_align: Align, size: RValue<'gcc>, flags: MemFlags) {
        if flags.contains(MemFlags::NONTEMPORAL) {
            // HACK(nox): This is inefficient but there is no nontemporal memmove.
            let val = self.load(src.get_type(), src, src_align);
            let ptr = self.pointercast(dst, self.type_ptr_to(self.val_ty(val)));
            self.store_with_flags(val, ptr, dst_align, flags);
            return;
        }
        let size = self.intcast(size, self.type_size_t(), false);
        let _is_volatile = flags.contains(MemFlags::VOLATILE);
        let dst = self.pointercast(dst, self.type_i8p());
        let src = self.pointercast(src, self.type_ptr_to(self.type_void()));

        let memmove = self.context.get_builtin_function("memmove");
        // TODO(antoyo): handle is_volatile.
        self.block.add_eval(None, self.context.new_call(None, memmove, &[dst, src, size]));
    }

    fn memset(&mut self, ptr: RValue<'gcc>, fill_byte: RValue<'gcc>, size: RValue<'gcc>, _align: Align, flags: MemFlags) {
        let _is_volatile = flags.contains(MemFlags::VOLATILE);
        let ptr = self.pointercast(ptr, self.type_i8p());
        let memset = self.context.get_builtin_function("memset");
        // TODO(antoyo): handle align and is_volatile.
        let fill_byte = self.context.new_cast(None, fill_byte, self.i32_type);
        let size = self.intcast(size, self.type_size_t(), false);
        self.block.add_eval(None, self.context.new_call(None, memset, &[ptr, fill_byte, size]));
    }

    fn select(&mut self, cond: RValue<'gcc>, then_val: RValue<'gcc>, mut else_val: RValue<'gcc>) -> RValue<'gcc> {
        let func = self.current_func();
        let variable = func.new_local(None, then_val.get_type(), "selectVar");
        let then_block = func.new_block("then");
        let else_block = func.new_block("else");
        let after_block = func.new_block("after");
        self.llbb().end_with_conditional(None, cond, then_block, else_block);

        then_block.add_assignment(None, variable, then_val);
        then_block.end_with_jump(None, after_block);

        if !then_val.get_type().is_compatible_with(else_val.get_type()) {
            else_val = self.context.new_cast(None, else_val, then_val.get_type());
        }
        else_block.add_assignment(None, variable, else_val);
        else_block.end_with_jump(None, after_block);

        // NOTE: since jumps were added in a place rustc does not expect, the current block in the
        // state need to be updated.
        self.switch_to_block(after_block);

        variable.to_rvalue()
    }

    #[allow(dead_code)]
    fn va_arg(&mut self, _list: RValue<'gcc>, _ty: Type<'gcc>) -> RValue<'gcc> {
        unimplemented!();
    }

    fn extract_element(&mut self, _vec: RValue<'gcc>, _idx: RValue<'gcc>) -> RValue<'gcc> {
        unimplemented!();
    }

    fn vector_splat(&mut self, _num_elts: usize, _elt: RValue<'gcc>) -> RValue<'gcc> {
        unimplemented!();
    }

    fn extract_value(&mut self, aggregate_value: RValue<'gcc>, idx: u64) -> RValue<'gcc> {
        // FIXME(antoyo): it would be better if the API only called this on struct, not on arrays.
        assert_eq!(idx as usize as u64, idx);
        let value_type = aggregate_value.get_type();

        if value_type.dyncast_array().is_some() {
            let index = self.context.new_rvalue_from_long(self.u64_type, i64::try_from(idx).expect("i64::try_from"));
            let element = self.context.new_array_access(None, aggregate_value, index);
            element.get_address(None)
        }
        else if value_type.dyncast_vector().is_some() {
            panic!();
        }
        else if let Some(pointer_type) = value_type.get_pointee() {
            if let Some(struct_type) = pointer_type.is_struct() {
                // NOTE: hack to workaround a limitation of the rustc API: see comment on
                // CodegenCx.structs_as_pointer
                aggregate_value.dereference_field(None, struct_type.get_field(idx as i32)).to_rvalue()
            }
            else {
                panic!("Unexpected type {:?}", value_type);
            }
        }
        else if let Some(struct_type) = value_type.is_struct() {
            aggregate_value.access_field(None, struct_type.get_field(idx as i32)).to_rvalue()
        }
        else {
            panic!("Unexpected type {:?}", value_type);
        }
    }

    fn insert_value(&mut self, aggregate_value: RValue<'gcc>, value: RValue<'gcc>, idx: u64) -> RValue<'gcc> {
        // FIXME(antoyo): it would be better if the API only called this on struct, not on arrays.
        assert_eq!(idx as usize as u64, idx);
        let value_type = aggregate_value.get_type();

        let lvalue =
            if value_type.dyncast_array().is_some() {
                let index = self.context.new_rvalue_from_long(self.u64_type, i64::try_from(idx).expect("i64::try_from"));
                self.context.new_array_access(None, aggregate_value, index)
            }
            else if value_type.dyncast_vector().is_some() {
                panic!();
            }
            else if let Some(pointer_type) = value_type.get_pointee() {
                if let Some(struct_type) = pointer_type.is_struct() {
                    // NOTE: hack to workaround a limitation of the rustc API: see comment on
                    // CodegenCx.structs_as_pointer
                    aggregate_value.dereference_field(None, struct_type.get_field(idx as i32))
                }
                else {
                    panic!("Unexpected type {:?}", value_type);
                }
            }
            else {
                panic!("Unexpected type {:?}", value_type);
            };

        let lvalue_type = lvalue.to_rvalue().get_type();
        let value =
            // NOTE: sometimes, rustc will create a value with the wrong type.
            if lvalue_type != value.get_type() {
                self.context.new_cast(None, value, lvalue_type)
            }
            else {
                value
            };

        self.llbb().add_assignment(None, lvalue, value);

        aggregate_value
    }

    fn set_personality_fn(&mut self, _personality: RValue<'gcc>) {
        // TODO(antoyo)
    }

    fn cleanup_landing_pad(&mut self, _ty: Type<'gcc>, _pers_fn: RValue<'gcc>) -> RValue<'gcc> {
        let field1 = self.context.new_field(None, self.u8_type.make_pointer(), "landing_pad_field_1");
        let field2 = self.context.new_field(None, self.i32_type, "landing_pad_field_1");
        let struct_type = self.context.new_struct_type(None, "landing_pad", &[field1, field2]);
        self.current_func().new_local(None, struct_type.as_type(), "landing_pad")
            .to_rvalue()
        // TODO(antoyo): Properly implement unwinding.
        // the above is just to make the compilation work as it seems
        // rustc_codegen_ssa now calls the unwinding builder methods even on panic=abort.
    }

    fn resume(&mut self, _exn: RValue<'gcc>) {
        // TODO(bjorn3): Properly implement unwinding.
        self.unreachable();
    }

    fn cleanup_pad(&mut self, _parent: Option<RValue<'gcc>>, _args: &[RValue<'gcc>]) -> Funclet {
        unimplemented!();
    }

    fn cleanup_ret(&mut self, _funclet: &Funclet, _unwind: Option<Block<'gcc>>) {
        unimplemented!();
    }

    fn catch_pad(&mut self, _parent: RValue<'gcc>, _args: &[RValue<'gcc>]) -> Funclet {
        unimplemented!();
    }

    fn catch_switch(
        &mut self,
        _parent: Option<RValue<'gcc>>,
        _unwind: Option<Block<'gcc>>,
        _handlers: &[Block<'gcc>],
    ) -> RValue<'gcc> {
        unimplemented!();
    }

    // Atomic Operations
    fn atomic_cmpxchg(&mut self, dst: RValue<'gcc>, cmp: RValue<'gcc>, src: RValue<'gcc>, order: AtomicOrdering, failure_order: AtomicOrdering, weak: bool) -> RValue<'gcc> {
        let expected = self.current_func().new_local(None, cmp.get_type(), "expected");
        self.llbb().add_assignment(None, expected, cmp);
        let success = self.compare_exchange(dst, expected, src, order, failure_order, weak);

        let pair_type = self.cx.type_struct(&[src.get_type(), self.bool_type], false);
        let result = self.current_func().new_local(None, pair_type, "atomic_cmpxchg_result");
        let align = Align::from_bits(64).expect("align"); // TODO(antoyo): use good align.

        let value_type = result.to_rvalue().get_type();
        if let Some(struct_type) = value_type.is_struct() {
            self.store(success, result.access_field(None, struct_type.get_field(1)).get_address(None), align);
            // NOTE: since success contains the call to the intrinsic, it must be stored before
            // expected so that we store expected after the call.
            self.store(expected.to_rvalue(), result.access_field(None, struct_type.get_field(0)).get_address(None), align);
        }
        // TODO(antoyo): handle when value is not a struct.

        result.to_rvalue()
    }

    fn atomic_rmw(&mut self, op: AtomicRmwBinOp, dst: RValue<'gcc>, src: RValue<'gcc>, order: AtomicOrdering) -> RValue<'gcc> {
        let size = src.get_type().get_size();
        let name =
            match op {
                AtomicRmwBinOp::AtomicXchg => format!("__atomic_exchange_{}", size),
                AtomicRmwBinOp::AtomicAdd => format!("__atomic_fetch_add_{}", size),
                AtomicRmwBinOp::AtomicSub => format!("__atomic_fetch_sub_{}", size),
                AtomicRmwBinOp::AtomicAnd => format!("__atomic_fetch_and_{}", size),
                AtomicRmwBinOp::AtomicNand => format!("__atomic_fetch_nand_{}", size),
                AtomicRmwBinOp::AtomicOr => format!("__atomic_fetch_or_{}", size),
                AtomicRmwBinOp::AtomicXor => format!("__atomic_fetch_xor_{}", size),
                AtomicRmwBinOp::AtomicMax => return self.atomic_extremum(ExtremumOperation::Max, dst, src, order),
                AtomicRmwBinOp::AtomicMin => return self.atomic_extremum(ExtremumOperation::Min, dst, src, order),
                AtomicRmwBinOp::AtomicUMax => return self.atomic_extremum(ExtremumOperation::Max, dst, src, order),
                AtomicRmwBinOp::AtomicUMin => return self.atomic_extremum(ExtremumOperation::Min, dst, src, order),
            };


        let atomic_function = self.context.get_builtin_function(name);
        let order = self.context.new_rvalue_from_int(self.i32_type, order.to_gcc());

        let void_ptr_type = self.context.new_type::<*mut ()>();
        let volatile_void_ptr_type = void_ptr_type.make_volatile();
        let dst = self.context.new_cast(None, dst, volatile_void_ptr_type);
        // FIXME(antoyo): not sure why, but we have the wrong type here.
        let new_src_type = atomic_function.get_param(1).to_rvalue().get_type();
        let src = self.context.new_cast(None, src, new_src_type);
        let res = self.context.new_call(None, atomic_function, &[dst, src, order]);
        self.context.new_cast(None, res, src.get_type())
    }

    fn atomic_fence(&mut self, order: AtomicOrdering, scope: SynchronizationScope) {
        let name =
            match scope {
                SynchronizationScope::SingleThread => "__atomic_signal_fence",
                SynchronizationScope::CrossThread => "__atomic_thread_fence",
            };
        let thread_fence = self.context.get_builtin_function(name);
        let order = self.context.new_rvalue_from_int(self.i32_type, order.to_gcc());
        self.llbb().add_eval(None, self.context.new_call(None, thread_fence, &[order]));
    }

    fn set_invariant_load(&mut self, load: RValue<'gcc>) {
        // NOTE: Hack to consider vtable function pointer as non-global-variable function pointer.
        self.normal_function_addresses.borrow_mut().insert(load);
        // TODO(antoyo)
    }

    fn lifetime_start(&mut self, _ptr: RValue<'gcc>, _size: Size) {
        // TODO(antoyo)
    }

    fn lifetime_end(&mut self, _ptr: RValue<'gcc>, _size: Size) {
        // TODO(antoyo)
    }

    fn call(&mut self, _typ: Type<'gcc>, func: RValue<'gcc>, args: &[RValue<'gcc>], funclet: Option<&Funclet>) -> RValue<'gcc> {
        // FIXME(antoyo): remove when having a proper API.
        let gcc_func = unsafe { std::mem::transmute(func) };
        if self.functions.borrow().values().find(|value| **value == gcc_func).is_some() {
            self.function_call(func, args, funclet)
        }
        else {
            // If it's a not function that was defined, it's a function pointer.
            self.function_ptr_call(func, args, funclet)
        }
    }

    fn zext(&mut self, value: RValue<'gcc>, dest_typ: Type<'gcc>) -> RValue<'gcc> {
        // FIXME(antoyo): this does not zero-extend.
        if value.get_type().is_bool() && dest_typ.is_i8(&self.cx) {
            // FIXME(antoyo): hack because base::from_immediate converts i1 to i8.
            // Fix the code in codegen_ssa::base::from_immediate.
            return value;
        }
        self.gcc_int_cast(value, dest_typ)
    }

    fn cx(&self) -> &CodegenCx<'gcc, 'tcx> {
        self.cx
    }

    fn do_not_inline(&mut self, _llret: RValue<'gcc>) {
        // FIMXE(bjorn3): implement
    }

    fn set_span(&mut self, _span: Span) {}

    fn from_immediate(&mut self, val: Self::Value) -> Self::Value {
        if self.cx().val_ty(val) == self.cx().type_i1() {
            self.zext(val, self.cx().type_i8())
        }
        else {
            val
        }
    }

    fn to_immediate_scalar(&mut self, val: Self::Value, scalar: abi::Scalar) -> Self::Value {
        if scalar.is_bool() {
            return self.trunc(val, self.cx().type_i1());
        }
        val
    }

    fn fptoui_sat(&mut self, _val: RValue<'gcc>, _dest_ty: Type<'gcc>) -> Option<RValue<'gcc>> {
        None
    }

    fn fptosi_sat(&mut self, _val: RValue<'gcc>, _dest_ty: Type<'gcc>) -> Option<RValue<'gcc>> {
        None
    }

    fn instrprof_increment(&mut self, _fn_name: RValue<'gcc>, _hash: RValue<'gcc>, _num_counters: RValue<'gcc>, _index: RValue<'gcc>) {
        unimplemented!();
    }
}

impl<'a, 'gcc, 'tcx> Builder<'a, 'gcc, 'tcx> {
    pub fn shuffle_vector(&mut self, v1: RValue<'gcc>, v2: RValue<'gcc>, mask: RValue<'gcc>) -> RValue<'gcc> {
        let struct_type = mask.get_type().is_struct().expect("mask of struct type");

        // TODO(antoyo): use a recursive unqualified() here.
        let vector_type = v1.get_type().unqualified().dyncast_vector().expect("vector type");
        let element_type = vector_type.get_element_type();
        let vec_num_units = vector_type.get_num_units();

        let mask_num_units = struct_type.get_field_count();
        let mut vector_elements = vec![];
        let mask_element_type =
            if element_type.is_integral() {
                element_type
            }
            else {
                self.int_type
            };
        for i in 0..mask_num_units {
            let field = struct_type.get_field(i as i32);
            vector_elements.push(self.context.new_cast(None, mask.access_field(None, field).to_rvalue(), mask_element_type));
        }

        // NOTE: the mask needs to be the same length as the input vectors, so add the missing
        // elements in the mask if needed.
        for _ in mask_num_units..vec_num_units {
            vector_elements.push(self.context.new_rvalue_zero(mask_element_type));
        }

        let array_type = self.context.new_array_type(None, element_type, vec_num_units as i32);
        let result_type = self.context.new_vector_type(element_type, mask_num_units as u64);
        let (v1, v2) =
            if vec_num_units < mask_num_units {
                // NOTE: the mask needs to be the same length as the input vectors, so join the 2
                // vectors and create a dummy second vector.
                // TODO(antoyo): switch to using new_vector_access.
                let array = self.context.new_bitcast(None, v1, array_type);
                let mut elements = vec![];
                for i in 0..vec_num_units {
                    elements.push(self.context.new_array_access(None, array, self.context.new_rvalue_from_int(self.int_type, i as i32)).to_rvalue());
                }
                // TODO(antoyo): switch to using new_vector_access.
                let array = self.context.new_bitcast(None, v2, array_type);
                for i in 0..vec_num_units {
                    elements.push(self.context.new_array_access(None, array, self.context.new_rvalue_from_int(self.int_type, i as i32)).to_rvalue());
                }
                let v1 = self.context.new_rvalue_from_vector(None, result_type, &elements);
                let zero = self.context.new_rvalue_zero(element_type);
                let v2 = self.context.new_rvalue_from_vector(None, result_type, &vec![zero; mask_num_units]);
                (v1, v2)
            }
            else {
                (v1, v2)
            };

        let new_mask_num_units = std::cmp::max(mask_num_units, vec_num_units);
        let mask_type = self.context.new_vector_type(mask_element_type, new_mask_num_units as u64);
        let mask = self.context.new_rvalue_from_vector(None, mask_type, &vector_elements);
        let result = self.context.new_rvalue_vector_perm(None, v1, v2, mask);

        if vec_num_units != mask_num_units {
            // NOTE: if padding was added, only select the number of elements of the masks to
            // remove that padding in the result.
            let mut elements = vec![];
            // TODO(antoyo): switch to using new_vector_access.
            let array = self.context.new_bitcast(None, result, array_type);
            for i in 0..mask_num_units {
                elements.push(self.context.new_array_access(None, array, self.context.new_rvalue_from_int(self.int_type, i as i32)).to_rvalue());
            }
            self.context.new_rvalue_from_vector(None, result_type, &elements)
        }
        else {
            result
        }
    }
}

impl<'a, 'gcc, 'tcx> StaticBuilderMethods for Builder<'a, 'gcc, 'tcx> {
    fn get_static(&mut self, def_id: DefId) -> RValue<'gcc> {
        // Forward to the `get_static` method of `CodegenCx`
        self.cx().get_static(def_id).get_address(None)
    }
}

impl<'tcx> HasParamEnv<'tcx> for Builder<'_, '_, 'tcx> {
    fn param_env(&self) -> ParamEnv<'tcx> {
        self.cx.param_env()
    }
}

impl<'tcx> HasTargetSpec for Builder<'_, '_, 'tcx> {
    fn target_spec(&self) -> &Target {
        &self.cx.target_spec()
    }
}

pub trait ToGccComp {
    fn to_gcc_comparison(&self) -> ComparisonOp;
}

impl ToGccComp for IntPredicate {
    fn to_gcc_comparison(&self) -> ComparisonOp {
        match *self {
            IntPredicate::IntEQ => ComparisonOp::Equals,
            IntPredicate::IntNE => ComparisonOp::NotEquals,
            IntPredicate::IntUGT => ComparisonOp::GreaterThan,
            IntPredicate::IntUGE => ComparisonOp::GreaterThanEquals,
            IntPredicate::IntULT => ComparisonOp::LessThan,
            IntPredicate::IntULE => ComparisonOp::LessThanEquals,
            IntPredicate::IntSGT => ComparisonOp::GreaterThan,
            IntPredicate::IntSGE => ComparisonOp::GreaterThanEquals,
            IntPredicate::IntSLT => ComparisonOp::LessThan,
            IntPredicate::IntSLE => ComparisonOp::LessThanEquals,
        }
    }
}

impl ToGccComp for RealPredicate {
    fn to_gcc_comparison(&self) -> ComparisonOp {
        // TODO(antoyo): check that ordered vs non-ordered is respected.
        match *self {
            RealPredicate::RealPredicateFalse => unreachable!(),
            RealPredicate::RealOEQ => ComparisonOp::Equals,
            RealPredicate::RealOGT => ComparisonOp::GreaterThan,
            RealPredicate::RealOGE => ComparisonOp::GreaterThanEquals,
            RealPredicate::RealOLT => ComparisonOp::LessThan,
            RealPredicate::RealOLE => ComparisonOp::LessThanEquals,
            RealPredicate::RealONE => ComparisonOp::NotEquals,
            RealPredicate::RealORD => unreachable!(),
            RealPredicate::RealUNO => unreachable!(),
            RealPredicate::RealUEQ => ComparisonOp::Equals,
            RealPredicate::RealUGT => ComparisonOp::GreaterThan,
            RealPredicate::RealUGE => ComparisonOp::GreaterThan,
            RealPredicate::RealULT => ComparisonOp::LessThan,
            RealPredicate::RealULE => ComparisonOp::LessThan,
            RealPredicate::RealUNE => ComparisonOp::NotEquals,
            RealPredicate::RealPredicateTrue => unreachable!(),
        }
    }
}

#[repr(C)]
#[allow(non_camel_case_types)]
enum MemOrdering {
    __ATOMIC_RELAXED,
    __ATOMIC_CONSUME,
    __ATOMIC_ACQUIRE,
    __ATOMIC_RELEASE,
    __ATOMIC_ACQ_REL,
    __ATOMIC_SEQ_CST,
}

trait ToGccOrdering {
    fn to_gcc(self) -> i32;
}

impl ToGccOrdering for AtomicOrdering {
    fn to_gcc(self) -> i32 {
        use MemOrdering::*;

        let ordering =
            match self {
                AtomicOrdering::NotAtomic => __ATOMIC_RELAXED, // TODO(antoyo): check if that's the same.
                AtomicOrdering::Unordered => __ATOMIC_RELAXED,
                AtomicOrdering::Monotonic => __ATOMIC_RELAXED, // TODO(antoyo): check if that's the same.
                AtomicOrdering::Acquire => __ATOMIC_ACQUIRE,
                AtomicOrdering::Release => __ATOMIC_RELEASE,
                AtomicOrdering::AcquireRelease => __ATOMIC_ACQ_REL,
                AtomicOrdering::SequentiallyConsistent => __ATOMIC_SEQ_CST,
            };
        ordering as i32
    }
}
