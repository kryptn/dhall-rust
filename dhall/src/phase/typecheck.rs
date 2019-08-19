use std::collections::HashMap;

use dhall_syntax::{
    rc, Builtin, Const, Expr, ExprF, InterpolatedTextContents, Label, SubExpr,
};

use crate::core::context::TypecheckContext;
use crate::core::value::TypedValue;
use crate::core::valuef::ValueF;
use crate::core::var::{Shift, Subst};
use crate::error::{TypeError, TypeMessage};
use crate::phase::Normalized;

fn tck_pi_type(
    ctx: &TypecheckContext,
    x: Label,
    tx: TypedValue,
    te: TypedValue,
) -> Result<TypedValue, TypeError> {
    use crate::error::TypeMessage::*;
    let ctx2 = ctx.insert_type(&x, tx.clone());

    let ka = match tx.get_type()?.as_const() {
        Some(k) => k,
        _ => return Err(TypeError::new(ctx, InvalidInputType(tx))),
    };

    let kb = match te.get_type()?.as_const() {
        Some(k) => k,
        _ => {
            return Err(TypeError::new(
                &ctx2,
                InvalidOutputType(te.get_type()?.into_owned()),
            ))
        }
    };

    let k = function_check(ka, kb);

    Ok(TypedValue::from_valuef_and_type(
        ValueF::Pi(x.into(), tx, te),
        TypedValue::from_const(k),
    ))
}

fn tck_record_type(
    ctx: &TypecheckContext,
    kts: impl IntoIterator<Item = Result<(Label, TypedValue), TypeError>>,
) -> Result<TypedValue, TypeError> {
    use crate::error::TypeMessage::*;
    use std::collections::hash_map::Entry;
    let mut new_kts = HashMap::new();
    // Check that all types are the same const
    let mut k = None;
    for e in kts {
        let (x, t) = e?;
        match (k, t.get_type()?.as_const()) {
            (None, Some(k2)) => k = Some(k2),
            (Some(k1), Some(k2)) if k1 == k2 => {}
            _ => return Err(TypeError::new(ctx, InvalidFieldType(x, t))),
        }
        let entry = new_kts.entry(x);
        match &entry {
            Entry::Occupied(_) => {
                return Err(TypeError::new(ctx, RecordTypeDuplicateField))
            }
            Entry::Vacant(_) => entry.or_insert_with(|| t),
        };
    }
    // An empty record type has type Type
    let k = k.unwrap_or(Const::Type);

    Ok(TypedValue::from_valuef_and_type(
        ValueF::RecordType(new_kts),
        TypedValue::from_const(k),
    ))
}

fn tck_union_type<Iter>(
    ctx: &TypecheckContext,
    kts: Iter,
) -> Result<TypedValue, TypeError>
where
    Iter: IntoIterator<Item = Result<(Label, Option<TypedValue>), TypeError>>,
{
    use crate::error::TypeMessage::*;
    use std::collections::hash_map::Entry;
    let mut new_kts = HashMap::new();
    // Check that all types are the same const
    let mut k = None;
    for e in kts {
        let (x, t) = e?;
        if let Some(t) = &t {
            match (k, t.get_type()?.as_const()) {
                (None, Some(k2)) => k = Some(k2),
                (Some(k1), Some(k2)) if k1 == k2 => {}
                _ => {
                    return Err(TypeError::new(
                        ctx,
                        InvalidFieldType(x, t.clone()),
                    ))
                }
            }
        }
        let entry = new_kts.entry(x);
        match &entry {
            Entry::Occupied(_) => {
                return Err(TypeError::new(ctx, UnionTypeDuplicateField))
            }
            Entry::Vacant(_) => entry.or_insert_with(|| t),
        };
    }

    // An empty union type has type Type;
    // an union type with only unary variants also has type Type
    let k = k.unwrap_or(Const::Type);

    Ok(TypedValue::from_valuef_and_type(
        ValueF::UnionType(new_kts),
        TypedValue::from_const(k),
    ))
}

fn function_check(a: Const, b: Const) -> Const {
    use std::cmp::max;
    if b == Const::Type {
        Const::Type
    } else {
        max(a, b)
    }
}

pub(crate) fn type_of_const(c: Const) -> Result<TypedValue, TypeError> {
    match c {
        Const::Type => Ok(TypedValue::from_const(Const::Kind)),
        Const::Kind => Ok(TypedValue::from_const(Const::Sort)),
        Const::Sort => {
            Err(TypeError::new(&TypecheckContext::new(), TypeMessage::Sort))
        }
    }
}

// Ad-hoc macro to help construct the types of builtins
macro_rules! make_type {
    (Type) => { ExprF::Const(Const::Type) };
    (Bool) => { ExprF::Builtin(Builtin::Bool) };
    (Natural) => { ExprF::Builtin(Builtin::Natural) };
    (Integer) => { ExprF::Builtin(Builtin::Integer) };
    (Double) => { ExprF::Builtin(Builtin::Double) };
    (Text) => { ExprF::Builtin(Builtin::Text) };
    ($var:ident) => {
        ExprF::Var(dhall_syntax::V(stringify!($var).into(), 0))
    };
    (Optional $ty:ident) => {
        ExprF::App(
            rc(ExprF::Builtin(Builtin::Optional)),
            rc(make_type!($ty))
        )
    };
    (List $($rest:tt)*) => {
        ExprF::App(
            rc(ExprF::Builtin(Builtin::List)),
            rc(make_type!($($rest)*))
        )
    };
    ({ $($label:ident : $ty:ident),* }) => {{
        let mut kts = dhall_syntax::map::DupTreeMap::new();
        $(
            kts.insert(
                Label::from(stringify!($label)),
                rc(make_type!($ty)),
            );
        )*
        ExprF::RecordType(kts)
    }};
    ($ty:ident -> $($rest:tt)*) => {
        ExprF::Pi(
            "_".into(),
            rc(make_type!($ty)),
            rc(make_type!($($rest)*))
        )
    };
    (($($arg:tt)*) -> $($rest:tt)*) => {
        ExprF::Pi(
            "_".into(),
            rc(make_type!($($arg)*)),
            rc(make_type!($($rest)*))
        )
    };
    (forall ($var:ident : $($ty:tt)*) -> $($rest:tt)*) => {
        ExprF::Pi(
            stringify!($var).into(),
            rc(make_type!($($ty)*)),
            rc(make_type!($($rest)*))
        )
    };
}

fn type_of_builtin<E>(b: Builtin) -> Expr<E> {
    use dhall_syntax::Builtin::*;
    match b {
        Bool | Natural | Integer | Double | Text => make_type!(Type),
        List | Optional => make_type!(
            Type -> Type
        ),

        NaturalFold => make_type!(
            Natural ->
            forall (natural: Type) ->
            forall (succ: natural -> natural) ->
            forall (zero: natural) ->
            natural
        ),
        NaturalBuild => make_type!(
            (forall (natural: Type) ->
                forall (succ: natural -> natural) ->
                forall (zero: natural) ->
                natural) ->
            Natural
        ),
        NaturalIsZero | NaturalEven | NaturalOdd => make_type!(
            Natural -> Bool
        ),
        NaturalToInteger => make_type!(Natural -> Integer),
        NaturalShow => make_type!(Natural -> Text),
        NaturalSubtract => make_type!(Natural -> Natural -> Natural),

        IntegerToDouble => make_type!(Integer -> Double),
        IntegerShow => make_type!(Integer -> Text),
        DoubleShow => make_type!(Double -> Text),
        TextShow => make_type!(Text -> Text),

        ListBuild => make_type!(
            forall (a: Type) ->
            (forall (list: Type) ->
                forall (cons: a -> list -> list) ->
                forall (nil: list) ->
                list) ->
            List a
        ),
        ListFold => make_type!(
            forall (a: Type) ->
            (List a) ->
            forall (list: Type) ->
            forall (cons: a -> list -> list) ->
            forall (nil: list) ->
            list
        ),
        ListLength => make_type!(forall (a: Type) -> (List a) -> Natural),
        ListHead | ListLast => {
            make_type!(forall (a: Type) -> (List a) -> Optional a)
        }
        ListIndexed => make_type!(
            forall (a: Type) ->
            (List a) ->
            List { index: Natural, value: a }
        ),
        ListReverse => make_type!(
            forall (a: Type) -> (List a) -> List a
        ),

        OptionalBuild => make_type!(
            forall (a: Type) ->
            (forall (optional: Type) ->
                forall (just: a -> optional) ->
                forall (nothing: optional) ->
                optional) ->
            Optional a
        ),
        OptionalFold => make_type!(
            forall (a: Type) ->
            (Optional a) ->
            forall (optional: Type) ->
            forall (just: a -> optional) ->
            forall (nothing: optional) ->
            optional
        ),
        OptionalNone => make_type!(
            forall (a: Type) -> Optional a
        ),
    }
}

// TODO: this can't fail in practise
pub(crate) fn builtin_to_type(b: Builtin) -> Result<TypedValue, TypeError> {
    type_with(&TypecheckContext::new(), SubExpr::from_builtin(b))
}

/// Intermediary return type
enum Ret {
    /// Returns the contained value as is
    RetWhole(TypedValue),
    /// Use the contained TypedValue as the type of the input expression
    RetTypeOnly(TypedValue),
}

/// Type-check an expression and return the expression alongside its type if type-checking
/// succeeded, or an error if type-checking failed.
/// Some normalization is done while typechecking, so the returned expression might be partially
/// normalized as well.
fn type_with(
    ctx: &TypecheckContext,
    e: SubExpr<Normalized>,
) -> Result<TypedValue, TypeError> {
    use dhall_syntax::ExprF::{Annot, Embed, Lam, Let, Pi, Var};

    use Ret::*;
    Ok(match e.as_ref() {
        Lam(x, t, b) => {
            let tx = type_with(ctx, t.clone())?;
            let ctx2 = ctx.insert_type(x, tx.clone());
            let b = type_with(&ctx2, b.clone())?;
            let v = ValueF::Lam(x.clone().into(), tx.clone(), b.to_value());
            let tb = b.get_type()?.into_owned();
            let t = tck_pi_type(ctx, x.clone(), tx, tb)?;
            TypedValue::from_valuef_and_type(v, t)
        }
        Pi(x, ta, tb) => {
            let ta = type_with(ctx, ta.clone())?;
            let ctx2 = ctx.insert_type(x, ta.clone());
            let tb = type_with(&ctx2, tb.clone())?;
            return tck_pi_type(ctx, x.clone(), ta, tb);
        }
        Let(x, t, v, e) => {
            let v = if let Some(t) = t {
                t.rewrap(Annot(v.clone(), t.clone()))
            } else {
                v.clone()
            };

            let v = type_with(ctx, v)?;
            return type_with(&ctx.insert_value(x, v.clone())?, e.clone());
        }
        Embed(p) => p.clone().into_typed().into_typedvalue(),
        Var(var) => match ctx.lookup(&var) {
            Some(typed) => typed.clone(),
            None => {
                return Err(TypeError::new(
                    ctx,
                    TypeMessage::UnboundVariable(var.clone()),
                ))
            }
        },
        _ => {
            // Typecheck recursively all subexpressions
            let expr =
                e.as_ref().traverse_ref_with_special_handling_of_binders(
                    |e| type_with(ctx, e.clone()),
                    |_, _| unreachable!(),
                )?;
            let ret = type_last_layer(ctx, &expr)?;
            match ret {
                RetTypeOnly(typ) => {
                    let expr = expr.map_ref(|typed| typed.to_value());
                    TypedValue::from_valuef_and_type(
                        ValueF::PartialExpr(expr),
                        typ,
                    )
                }
                RetWhole(tt) => tt,
            }
        }
    })
}

/// When all sub-expressions have been typed, check the remaining toplevel
/// layer.
fn type_last_layer(
    ctx: &TypecheckContext,
    e: &ExprF<TypedValue, Normalized>,
) -> Result<Ret, TypeError> {
    use crate::error::TypeMessage::*;
    use dhall_syntax::BinOp::*;
    use dhall_syntax::Builtin::*;
    use dhall_syntax::Const::Type;
    use dhall_syntax::ExprF::*;
    use Ret::*;
    let mkerr = |msg: TypeMessage| TypeError::new(ctx, msg);

    match e {
        Import(_) => unreachable!(
            "There should remain no imports in a resolved expression"
        ),
        Lam(_, _, _) | Pi(_, _, _) | Let(_, _, _, _) | Embed(_) | Var(_) => {
            unreachable!()
        }
        App(f, a) => {
            let tf = f.get_type()?;
            let tf_borrow = tf.as_whnf();
            let (x, tx, tb) = match &*tf_borrow {
                ValueF::Pi(x, tx, tb) => (x, tx, tb),
                _ => return Err(mkerr(NotAFunction(f.clone()))),
            };
            if a.get_type()?.as_ref() != tx {
                return Err(mkerr(TypeMismatch(
                    f.clone(),
                    tx.clone(),
                    a.clone(),
                )));
            }

            Ok(RetTypeOnly(tb.subst_shift(&x.into(), a)))
        }
        Annot(x, t) => {
            if t != x.get_type()?.as_ref() {
                return Err(mkerr(AnnotMismatch(x.clone(), t.clone())));
            }
            Ok(RetTypeOnly(x.get_type()?.into_owned()))
        }
        Assert(t) => {
            match &*t.as_whnf() {
                ValueF::Equivalence(x, y) if x == y => {}
                ValueF::Equivalence(x, y) => {
                    return Err(mkerr(AssertMismatch(x.clone(), y.clone())))
                }
                _ => return Err(mkerr(AssertMustTakeEquivalence)),
            }
            Ok(RetTypeOnly(t.clone()))
        }
        BoolIf(x, y, z) => {
            if x.get_type()?.as_ref() != &builtin_to_type(Bool)? {
                return Err(mkerr(InvalidPredicate(x.clone())));
            }

            if y.get_type()?.get_type()?.as_const() != Some(Type) {
                return Err(mkerr(IfBranchMustBeTerm(true, y.clone())));
            }

            if z.get_type()?.get_type()?.as_const() != Some(Type) {
                return Err(mkerr(IfBranchMustBeTerm(false, z.clone())));
            }

            if y.get_type()? != z.get_type()? {
                return Err(mkerr(IfBranchMismatch(y.clone(), z.clone())));
            }

            Ok(RetTypeOnly(y.get_type()?.into_owned()))
        }
        EmptyListLit(t) => {
            match &*t.as_whnf() {
                ValueF::AppliedBuiltin(dhall_syntax::Builtin::List, args)
                    if args.len() == 1 => {}
                _ => {
                    return Err(TypeError::new(ctx, InvalidListType(t.clone())))
                }
            }
            Ok(RetTypeOnly(t.clone()))
        }
        NEListLit(xs) => {
            let mut iter = xs.iter().enumerate();
            let (_, x) = iter.next().unwrap();
            for (i, y) in iter {
                if x.get_type()? != y.get_type()? {
                    return Err(mkerr(InvalidListElement(
                        i,
                        x.get_type()?.into_owned(),
                        y.clone(),
                    )));
                }
            }
            let t = x.get_type()?;
            if t.get_type()?.as_const() != Some(Type) {
                return Err(TypeError::new(
                    ctx,
                    InvalidListType(t.into_owned()),
                ));
            }

            Ok(RetTypeOnly(TypedValue::from_valuef_and_type(
                ValueF::from_builtin(dhall_syntax::Builtin::List)
                    .app_value(t.to_value()),
                TypedValue::from_const(Type),
            )))
        }
        SomeLit(x) => {
            let t = x.get_type()?.into_owned();
            if t.get_type()?.as_const() != Some(Type) {
                return Err(TypeError::new(ctx, InvalidOptionalType(t)));
            }

            Ok(RetTypeOnly(TypedValue::from_valuef_and_type(
                ValueF::from_builtin(dhall_syntax::Builtin::Optional)
                    .app_value(t.to_value()),
                TypedValue::from_const(Type),
            )))
        }
        RecordType(kts) => Ok(RetWhole(tck_record_type(
            ctx,
            kts.iter().map(|(x, t)| Ok((x.clone(), t.clone()))),
        )?)),
        UnionType(kts) => Ok(RetWhole(tck_union_type(
            ctx,
            kts.iter().map(|(x, t)| Ok((x.clone(), t.clone()))),
        )?)),
        RecordLit(kvs) => Ok(RetTypeOnly(tck_record_type(
            ctx,
            kvs.iter()
                .map(|(x, v)| Ok((x.clone(), v.get_type()?.into_owned()))),
        )?)),
        Field(r, x) => {
            match &*r.get_type()?.as_whnf() {
                ValueF::RecordType(kts) => match kts.get(&x) {
                    Some(tth) => {
                        Ok(RetTypeOnly(tth.clone()))
                    },
                    None => Err(mkerr(MissingRecordField(x.clone(),
                                        r.clone()))),
                },
                // TODO: branch here only when r.get_type() is a Const
                _ => {
                    match &*r.as_whnf() {
                        ValueF::UnionType(kts) => match kts.get(&x) {
                            // Constructor has type T -> < x: T, ... >
                            Some(Some(t)) => {
                                Ok(RetTypeOnly(
                                    tck_pi_type(
                                        ctx,
                                        "_".into(),
                                        t.clone(),
                                        r.under_binder(Label::from("_")),
                                    )?
                                ))
                            },
                            Some(None) => {
                                Ok(RetTypeOnly(r.clone()))
                            },
                            None => {
                                Err(mkerr(MissingUnionField(
                                    x.clone(),
                                    r.clone(),
                                )))
                            },
                        },
                        _ => {
                            Err(mkerr(NotARecord(
                                x.clone(),
                                r.clone()
                            )))
                        },
                    }
                }
                // _ => Err(mkerr(NotARecord(
                //     x,
                //     r?,
                // ))),
            }
        }
        Const(c) => Ok(RetWhole(TypedValue::from_const(*c))),
        Builtin(b) => Ok(RetTypeOnly(type_with(ctx, rc(type_of_builtin(*b)))?)),
        BoolLit(_) => Ok(RetTypeOnly(builtin_to_type(Bool)?)),
        NaturalLit(_) => Ok(RetTypeOnly(builtin_to_type(Natural)?)),
        IntegerLit(_) => Ok(RetTypeOnly(builtin_to_type(Integer)?)),
        DoubleLit(_) => Ok(RetTypeOnly(builtin_to_type(Double)?)),
        TextLit(interpolated) => {
            let text_type = builtin_to_type(Text)?;
            for contents in interpolated.iter() {
                use InterpolatedTextContents::Expr;
                if let Expr(x) = contents {
                    if x.get_type()?.as_ref() != &text_type {
                        return Err(mkerr(InvalidTextInterpolation(x.clone())));
                    }
                }
            }
            Ok(RetTypeOnly(text_type))
        }
        BinOp(RightBiasedRecordMerge, l, r) => {
            use crate::phase::normalize::merge_maps;

            let l_type = l.get_type()?;
            let l_kind = l_type.get_type()?;
            let r_type = r.get_type()?;
            let r_kind = r_type.get_type()?;

            // Check the equality of kinds.
            // This is to disallow expression such as:
            // "{ x = Text } // { y = 1 }"
            if l_kind != r_kind {
                return Err(mkerr(RecordMismatch(l.clone(), r.clone())));
            }

            // Extract the LHS record type
            let l_type_borrow = l_type.as_whnf();
            let kts_x = match &*l_type_borrow {
                ValueF::RecordType(kts) => kts,
                _ => return Err(mkerr(MustCombineRecord(l.clone()))),
            };

            // Extract the RHS record type
            let r_type_borrow = r_type.as_whnf();
            let kts_y = match &*r_type_borrow {
                ValueF::RecordType(kts) => kts,
                _ => return Err(mkerr(MustCombineRecord(r.clone()))),
            };

            // Union the two records, prefering
            // the values found in the RHS.
            let kts = merge_maps(kts_x, kts_y, |_, r_t| r_t.clone());

            // Construct the final record type from the union
            Ok(RetTypeOnly(tck_record_type(
                ctx,
                kts.into_iter().map(|(x, v)| Ok((x.clone(), v))),
            )?))
        }
        BinOp(RecursiveRecordMerge, l, r) => {
            // A recursive function to dig down into
            // records of records when merging.
            fn combine_record_types(
                ctx: &TypecheckContext,
                kts_l: &HashMap<Label, TypedValue>,
                kts_r: &HashMap<Label, TypedValue>,
            ) -> Result<TypedValue, TypeError> {
                use crate::phase::normalize::outer_join;

                // If the Label exists for both records and the values
                // are records themselves, then we hit the recursive case.
                // Otherwise we have a field collision.
                let combine =
                    |k: &Label,
                     inner_l: &TypedValue,
                     inner_r: &TypedValue|
                     -> Result<TypedValue, TypeError> {
                        match (&*inner_l.as_whnf(), &*inner_r.as_whnf()) {
                            (
                                ValueF::RecordType(inner_l_kvs),
                                ValueF::RecordType(inner_r_kvs),
                            ) => combine_record_types(
                                ctx,
                                inner_l_kvs,
                                inner_r_kvs,
                            ),
                            (_, _) => Err(TypeError::new(
                                ctx,
                                FieldCollision(k.clone()),
                            )),
                        }
                    };

                let kts: HashMap<Label, Result<TypedValue, TypeError>> =
                    outer_join(
                        |l| Ok(l.clone()),
                        |r| Ok(r.clone()),
                        combine,
                        kts_l,
                        kts_r,
                    );

                Ok(tck_record_type(
                    ctx,
                    kts.into_iter().map(|(x, v)| v.map(|r| (x.clone(), r))),
                )?)
            };

            let l_type = l.get_type()?;
            let l_kind = l_type.get_type()?;
            let r_type = r.get_type()?;
            let r_kind = r_type.get_type()?;

            // Check the equality of kinds.
            // This is to disallow expression such as:
            // "{ x = Text } // { y = 1 }"
            if l_kind != r_kind {
                return Err(mkerr(RecordMismatch(l.clone(), r.clone())));
            }

            // Extract the LHS record type
            let l_type_borrow = l_type.as_whnf();
            let kts_x = match &*l_type_borrow {
                ValueF::RecordType(kts) => kts,
                _ => return Err(mkerr(MustCombineRecord(l.clone()))),
            };

            // Extract the RHS record type
            let r_type_borrow = r_type.as_whnf();
            let kts_y = match &*r_type_borrow {
                ValueF::RecordType(kts) => kts,
                _ => return Err(mkerr(MustCombineRecord(r.clone()))),
            };

            combine_record_types(ctx, kts_x, kts_y).map(|r| RetTypeOnly(r))
        }
        BinOp(RecursiveRecordTypeMerge, l, r) => {
            // A recursive function to dig down into
            // records of records when merging.
            fn combine_record_types(
                ctx: &TypecheckContext,
                kts_l: &HashMap<Label, TypedValue>,
                kts_r: &HashMap<Label, TypedValue>,
            ) -> Result<TypedValue, TypeError> {
                use crate::phase::normalize::intersection_with_key;

                // If the Label exists for both records and the values
                // are records themselves, then we hit the recursive case.
                // Otherwise we have a field collision.
                let combine =
                    |k: &Label,
                     kts_l_inner: &TypedValue,
                     kts_r_inner: &TypedValue|
                     -> Result<TypedValue, TypeError> {
                        match (&*kts_l_inner.as_whnf(), &*kts_r_inner.as_whnf())
                        {
                            (
                                ValueF::RecordType(kvs_l_inner),
                                ValueF::RecordType(kvs_r_inner),
                            ) => combine_record_types(
                                ctx,
                                kvs_l_inner,
                                kvs_r_inner,
                            ),
                            (_, _) => Err(TypeError::new(
                                ctx,
                                FieldCollision(k.clone()),
                            )),
                        }
                    };

                let kts = intersection_with_key(combine, kts_l, kts_r);

                Ok(tck_record_type(
                    ctx,
                    kts.into_iter().map(|(x, v)| v.map(|r| (x.clone(), r))),
                )?)
            };

            // Extract the Const of the LHS
            let k_l = match l.get_type()?.as_const() {
                Some(k) => k,
                _ => {
                    return Err(mkerr(RecordTypeMergeRequiresRecordType(
                        l.clone(),
                    )))
                }
            };

            // Extract the Const of the RHS
            let k_r = match r.get_type()?.as_const() {
                Some(k) => k,
                _ => {
                    return Err(mkerr(RecordTypeMergeRequiresRecordType(
                        r.clone(),
                    )))
                }
            };

            // Const values must match for the Records
            let k = if k_l == k_r {
                k_l
            } else {
                return Err(mkerr(RecordTypeMismatch(
                    TypedValue::from_const(k_l),
                    TypedValue::from_const(k_r),
                    l.clone(),
                    r.clone(),
                )));
            };

            // Extract the LHS record type
            let borrow_l = l.as_whnf();
            let kts_x = match &*borrow_l {
                ValueF::RecordType(kts) => kts,
                _ => {
                    return Err(mkerr(RecordTypeMergeRequiresRecordType(
                        l.clone(),
                    )))
                }
            };

            // Extract the RHS record type
            let borrow_r = r.as_whnf();
            let kts_y = match &*borrow_r {
                ValueF::RecordType(kts) => kts,
                _ => {
                    return Err(mkerr(RecordTypeMergeRequiresRecordType(
                        r.clone(),
                    )))
                }
            };

            // Ensure that the records combine without a type error
            // and if not output the final Const value.
            combine_record_types(ctx, kts_x, kts_y)
                .and(Ok(RetTypeOnly(TypedValue::from_const(k))))
        }
        BinOp(o @ ListAppend, l, r) => {
            match &*l.get_type()?.as_whnf() {
                ValueF::AppliedBuiltin(List, _) => {}
                _ => return Err(mkerr(BinOpTypeMismatch(*o, l.clone()))),
            }

            if l.get_type()? != r.get_type()? {
                return Err(mkerr(BinOpTypeMismatch(*o, r.clone())));
            }

            Ok(RetTypeOnly(l.get_type()?.into_owned()))
        }
        BinOp(Equivalence, l, r) => {
            if l.get_type()?.get_type()?.as_const() != Some(Type) {
                return Err(mkerr(EquivalenceArgumentMustBeTerm(
                    true,
                    l.clone(),
                )));
            }
            if r.get_type()?.get_type()?.as_const() != Some(Type) {
                return Err(mkerr(EquivalenceArgumentMustBeTerm(
                    false,
                    r.clone(),
                )));
            }

            if l.get_type()? != r.get_type()? {
                return Err(mkerr(EquivalenceTypeMismatch(
                    r.clone(),
                    l.clone(),
                )));
            }

            Ok(RetTypeOnly(TypedValue::from_const(Type)))
        }
        BinOp(o, l, r) => {
            let t = builtin_to_type(match o {
                BoolAnd => Bool,
                BoolOr => Bool,
                BoolEQ => Bool,
                BoolNE => Bool,
                NaturalPlus => Natural,
                NaturalTimes => Natural,
                TextAppend => Text,
                ListAppend => unreachable!(),
                RightBiasedRecordMerge => unreachable!(),
                RecursiveRecordMerge => unreachable!(),
                RecursiveRecordTypeMerge => unreachable!(),
                ImportAlt => unreachable!("There should remain no import alternatives in a resolved expression"),
                Equivalence => unreachable!(),
            })?;

            if l.get_type()?.as_ref() != &t {
                return Err(mkerr(BinOpTypeMismatch(*o, l.clone())));
            }

            if r.get_type()?.as_ref() != &t {
                return Err(mkerr(BinOpTypeMismatch(*o, r.clone())));
            }

            Ok(RetTypeOnly(t))
        }
        Merge(record, union, type_annot) => {
            let record_type = record.get_type()?;
            let record_borrow = record_type.as_whnf();
            let handlers = match &*record_borrow {
                ValueF::RecordType(kts) => kts,
                _ => return Err(mkerr(Merge1ArgMustBeRecord(record.clone()))),
            };

            let union_type = union.get_type()?;
            let union_borrow = union_type.as_whnf();
            let variants = match &*union_borrow {
                ValueF::UnionType(kts) => kts,
                _ => return Err(mkerr(Merge2ArgMustBeUnion(union.clone()))),
            };

            let mut inferred_type = None;
            for (x, handler_type) in handlers {
                let handler_return_type =
                    match variants.get(x) {
                        // Union alternative with type
                        Some(Some(variant_type)) => {
                            let handler_type_borrow = handler_type.as_whnf();
                            let (x, tx, tb) = match &*handler_type_borrow {
                                ValueF::Pi(x, tx, tb) => (x, tx, tb),
                                _ => {
                                    return Err(mkerr(NotAFunction(
                                        handler_type.clone(),
                                    )))
                                }
                            };

                            if variant_type != tx {
                                return Err(mkerr(TypeMismatch(
                                    handler_type.clone(),
                                    tx.clone(),
                                    variant_type.clone(),
                                )));
                            }

                            // Extract `tb` from under the `x` binder. Fails is `x` was free in `tb`.
                            match tb.over_binder(x) {
                                Some(x) => x,
                                None => return Err(mkerr(
                                    MergeHandlerReturnTypeMustNotBeDependent,
                                )),
                            }
                        }
                        // Union alternative without type
                        Some(None) => handler_type.clone(),
                        None => {
                            return Err(mkerr(MergeHandlerMissingVariant(
                                x.clone(),
                            )))
                        }
                    };
                match &inferred_type {
                    None => inferred_type = Some(handler_return_type),
                    Some(t) => {
                        if t != &handler_return_type {
                            return Err(mkerr(MergeHandlerTypeMismatch));
                        }
                    }
                }
            }
            for x in variants.keys() {
                if !handlers.contains_key(x) {
                    return Err(mkerr(MergeVariantMissingHandler(x.clone())));
                }
            }

            match (inferred_type, type_annot) {
                (Some(ref t1), Some(t2)) => {
                    if t1 != t2 {
                        return Err(mkerr(MergeAnnotMismatch));
                    }
                    Ok(RetTypeOnly(t2.clone()))
                }
                (Some(t), None) => Ok(RetTypeOnly(t)),
                (None, Some(t)) => Ok(RetTypeOnly(t.clone())),
                (None, None) => Err(mkerr(MergeEmptyNeedsAnnotation)),
            }
        }
        Projection(record, labels) => {
            let record_type = record.get_type()?;
            let record_borrow = record_type.as_whnf();
            let kts = match &*record_borrow {
                ValueF::RecordType(kts) => kts,
                _ => return Err(mkerr(ProjectionMustBeRecord)),
            };

            let mut new_kts = HashMap::new();
            for l in labels {
                match kts.get(l) {
                    None => return Err(mkerr(ProjectionMissingEntry)),
                    Some(t) => new_kts.insert(l.clone(), t.clone()),
                };
            }

            Ok(RetTypeOnly(TypedValue::from_valuef_and_type(
                ValueF::RecordType(new_kts),
                record_type.get_type()?.into_owned(),
            )))
        }
    }
}

/// `type_of` is the same as `type_with` with an empty context, meaning that the
/// expression must be closed (i.e. no free variables), otherwise type-checking
/// will fail.
pub(crate) fn typecheck(
    e: SubExpr<Normalized>,
) -> Result<TypedValue, TypeError> {
    type_with(&TypecheckContext::new(), e)
}

pub(crate) fn typecheck_with(
    expr: SubExpr<Normalized>,
    ty: SubExpr<Normalized>,
) -> Result<TypedValue, TypeError> {
    typecheck(expr.rewrap(ExprF::Annot(expr.clone(), ty)))
}
