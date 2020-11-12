// The reducer reduces the program to a single function which is:
// - in SSA form
// - free of function calls (except for low level calls) thanks to inlining
// - free of for-loops thanks to unrolling

// The process happens in two steps
// 1. Shallow SSA for the `main` function
// We turn the `main` function into SSA form, but ignoring function calls and for loops
// 2. Unroll and inline
// We go through the shallow-SSA program and
// - unroll loops
// - inline function calls. This includes applying shallow-ssa on the target function

mod inline;
mod shallow_ssa;
mod unroll;

use self::inline::{inline_call, InlineError};
use std::collections::HashMap;
use typed_absy::result_folder::*;
use typed_absy::types::GenericsAssignment;
use typed_absy::Folder;

use typed_absy::{
    ArrayExpression, ArrayExpressionInner, BooleanExpression, ConcreteFunctionKey, CoreIdentifier,
    DeclarationFunctionKey, FieldElementExpression, FunctionCall, Identifier, StructExpression,
    StructExpressionInner, Type, Typed, TypedExpression, TypedExpressionList, TypedFunction,
    TypedFunctionSymbol, TypedModule, TypedModuleId, TypedProgram, TypedStatement, UExpression,
    UExpressionInner, Variable,
};

use std::convert::{TryFrom, TryInto};

use zokrates_field::Field;

use self::shallow_ssa::ShallowTransformer;

use static_analysis::Propagator;

use std::fmt;

// An SSA version map, giving access to the latest version number for each identifier
pub type Versions<'ast> = HashMap<CoreIdentifier<'ast>, usize>;

// A container to represent whether more treatment must be applied to the function
#[derive(Debug, PartialEq)]
pub enum Output<U, V> {
    Complete(U),
    Incomplete(U, V),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    Incompatible(String, String),
    GenericsInMain,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Incompatible(decl, conc) => write!(
                f,
                "Call site `{}` incompatible with declaration `{}`",
                conc, decl
            ),
            Error::GenericsInMain => write!(f, "Cannot generate code for generic function"),
        }
    }
}

type CallCache<'ast, T> = HashMap<
    (ConcreteFunctionKey<'ast>, Vec<TypedExpression<'ast, T>>),
    Vec<TypedExpression<'ast, T>>,
>;

#[derive(Debug, Default)]
struct Substitutions<'ast>(HashMap<CoreIdentifier<'ast>, HashMap<usize, usize>>);

impl<'ast> Substitutions<'ast> {
    // create an equivalent substitution map where all paths
    // are of length 1
    fn canonicalize(self) -> Self {
        Substitutions(
            self.0
                .into_iter()
                .map(|(id, sub)| (id, Self::canonicalize_sub(sub)))
                .collect(),
        )
    }

    // canonicalize substitutions for a given id
    fn canonicalize_sub(sub: HashMap<usize, usize>) -> HashMap<usize, usize> {
        fn add_to_cache(
            sub: &HashMap<usize, usize>,
            cache: HashMap<usize, usize>,
            k: usize,
        ) -> HashMap<usize, usize> {
            match cache.contains_key(&k) {
                // `k` is already in the cache, no changes to the cache
                true => cache,
                _ => match sub.get(&k) {
                    // `k` does not point to anything, no changes to the cache
                    None => cache,
                    // `k` points to some `v
                    Some(v) => {
                        // add `v` to the cache
                        let cache = add_to_cache(sub, cache, *v);
                        // `k` points to what `v` points to, or to `v`
                        let v = cache.get(v).cloned().unwrap_or(*v);
                        let mut cache = cache;
                        cache.insert(k, v);
                        cache
                    }
                },
            }
        }

        sub.keys()
            .fold(HashMap::new(), |cache, k| add_to_cache(&sub, cache, *k))
    }
}

struct Sub<'a, 'ast> {
    substitutions: &'a Substitutions<'ast>,
}

impl<'a, 'ast> Sub<'a, 'ast> {
    fn new(substitutions: &'a Substitutions<'ast>) -> Self {
        Self { substitutions }
    }
}

impl<'a, 'ast, T: Field> Folder<'ast, T> for Sub<'a, 'ast> {
    fn fold_name(&mut self, id: Identifier<'ast>) -> Identifier<'ast> {
        let version = self
            .substitutions
            .0
            .get(&id.id)
            .map(|sub| sub.get(&id.version).cloned().unwrap_or(id.version))
            .unwrap_or(id.version);
        id.version(version)
    }
}

fn register<'ast>(
    substitutions: &mut Substitutions<'ast>,
    substitute: &Versions<'ast>,
    with: &Versions<'ast>,
) {
    for (id, key, value) in substitute
        .iter()
        .filter_map(|(id, version)| with.get(&id).clone().map(|to| (id, version, to)))
        .filter(|(_, key, value)| key != value)
    {
        let sub = substitutions.0.entry(id.clone()).or_default();

        // redirect `k` to `v`, unless `v` is already redirected to `v0`, in which case we redirect to `v0`

        sub.insert(*key, *sub.get(value).unwrap_or(value));
    }
}

fn embeds_in_module<'ast, T: Field>(
    module_id: &TypedModuleId,
) -> Vec<(DeclarationFunctionKey<'ast>, TypedFunctionSymbol<'ast, T>)> {
    // define a function in the embed module for the `unpack` embed
    let unpack = crate::embed::FlatEmbed::Unpack(T::get_required_bits());
    let unpack_key = unpack.key_in_module::<T>(module_id);

    // define a function in the embed module for the `u32_to_bits` embed
    let u32_to_bits = crate::embed::FlatEmbed::U32ToBits;
    let u32_to_bits_key = u32_to_bits.key_in_module::<T>(module_id);

    // define a function in the embed module for the `u16_to_bits` embed
    let u16_to_bits = crate::embed::FlatEmbed::U16ToBits;
    let u16_to_bits_key = u16_to_bits.key_in_module::<T>(module_id);

    // define a function in the embed module for the `u8_to_bits` embed
    let u8_to_bits = crate::embed::FlatEmbed::U8ToBits;
    let u8_to_bits_key = u8_to_bits.key_in_module::<T>(module_id);

    // define a function in the embed module for the `u32_from_bits` embed
    let u32_from_bits = crate::embed::FlatEmbed::U32FromBits;
    let u32_from_bits_key = u32_from_bits.key_in_module::<T>(module_id);

    // define a function in the embed module for the `u16_from_bits` embed
    let u16_from_bits = crate::embed::FlatEmbed::U16FromBits;
    let u16_from_bits_key = u16_from_bits.key_in_module::<T>(module_id);

    // define a function in the embed module for the `u8_from_bits` embed
    let u8_from_bits = crate::embed::FlatEmbed::U8FromBits;
    let u8_from_bits_key = u8_from_bits.key_in_module::<T>(module_id);

    vec![
        (unpack_key.into(), TypedFunctionSymbol::Flat(unpack)),
        (
            u32_from_bits_key.into(),
            TypedFunctionSymbol::Flat(u32_from_bits),
        ),
        (
            u16_from_bits_key.into(),
            TypedFunctionSymbol::Flat(u16_from_bits),
        ),
        (
            u8_from_bits_key.into(),
            TypedFunctionSymbol::Flat(u8_from_bits),
        ),
        (
            u32_to_bits_key.into(),
            TypedFunctionSymbol::Flat(u32_to_bits),
        ),
        (
            u16_to_bits_key.into(),
            TypedFunctionSymbol::Flat(u16_to_bits),
        ),
        (u8_to_bits_key.into(), TypedFunctionSymbol::Flat(u8_to_bits)),
    ]
}

struct Reducer<'ast, 'a, T> {
    statement_buffer: Vec<TypedStatement<'ast, T>>,
    for_loop_versions: Vec<Versions<'ast>>,
    for_loop_versions_after: Vec<Versions<'ast>>,
    program: &'a TypedProgram<'ast, T>,
    versions: &'a mut Versions<'ast>,
    substitutions: &'a mut Substitutions<'ast>,
    cache: CallCache<'ast, T>,
    complete: bool,
}

impl<'ast, 'a, T: Field> Reducer<'ast, 'a, T> {
    fn new(
        program: &'a TypedProgram<'ast, T>,
        versions: &'a mut Versions<'ast>,
        substitutions: &'a mut Substitutions<'ast>,
        for_loop_versions: Vec<Versions<'ast>>,
    ) -> Self {
        // we reverse the vector as it's cheaper to `pop` than to take from
        // the head
        let mut for_loop_versions = for_loop_versions;

        for_loop_versions.reverse();

        Reducer {
            statement_buffer: vec![],
            for_loop_versions_after: vec![],
            for_loop_versions,
            cache: CallCache::default(),
            substitutions,
            program,
            versions,
            complete: true,
        }
    }

    fn fold_function_call<E>(
        &mut self,
        key: DeclarationFunctionKey<'ast>,
        arguments: Vec<TypedExpression<'ast, T>>,
        output_types: Vec<Type<'ast, T>>,
    ) -> Result<E, Error>
    where
        E: FunctionCall<'ast, T> + TryFrom<TypedExpression<'ast, T>, Error = ()> + std::fmt::Debug,
    {
        let arguments = arguments
            .into_iter()
            .map(|e| self.fold_expression(e))
            .collect::<Result<_, _>>()?;
        let res = inline_call(
            key.clone(),
            arguments,
            output_types,
            &self.program,
            &mut self.cache,
            &mut self.versions,
        );

        match res {
            Ok(Output::Complete((statements, expressions))) => {
                self.complete &= true;
                self.statement_buffer.extend(statements);
                Ok(expressions[0].clone().try_into().unwrap())
            }
            Ok(Output::Incomplete((statements, expressions), delta_for_loop_versions)) => {
                self.complete = false;
                self.statement_buffer.extend(statements);
                self.for_loop_versions_after.extend(delta_for_loop_versions);
                Ok(expressions[0].clone().try_into().unwrap())
            }
            Err(InlineError::Generic(decl, conc)) => {
                Err(Error::Incompatible(decl.to_string(), conc.to_string()))
            }
            Err(InlineError::NonConstant(key, arguments, _)) => {
                self.complete = false;

                Ok(E::function_call(key, arguments))
            }
            Err(InlineError::Flat(embed, arguments, output_types)) => {
                let identifier = Identifier::from(CoreIdentifier::Call(0)).version(
                    *self
                        .versions
                        .entry(CoreIdentifier::Call(0).clone())
                        .and_modify(|e| *e += 1) // if it was already declared, we increment
                        .or_insert(0),
                );
                let var = Variable::with_id_and_type(identifier, output_types[0].clone());

                let v = vec![var.clone()];

                self.statement_buffer
                    .push(TypedStatement::MultipleDefinition(
                        v,
                        TypedExpressionList::FunctionCall(
                            embed.key_in_module::<T>(&self.program.main).into(),
                            arguments,
                            output_types,
                        ),
                    ));
                Ok(TypedExpression::from(var).try_into().unwrap())
            }
        }
    }
}

impl<'ast, 'a, T: Field> ResultFolder<'ast, T> for Reducer<'ast, 'a, T> {
    type Error = Error;

    fn fold_statement(
        &mut self,
        s: TypedStatement<'ast, T>,
    ) -> Result<Vec<TypedStatement<'ast, T>>, Self::Error> {
        let res = match s {
            TypedStatement::MultipleDefinition(
                v,
                TypedExpressionList::FunctionCall(key, arguments, output_types),
            ) => {
                let arguments = arguments
                    .into_iter()
                    .map(|a| self.fold_expression(a))
                    .collect::<Result<_, _>>()?;

                match inline_call(
                    key,
                    arguments,
                    output_types,
                    &self.program,
                    &mut self.cache,
                    &mut self.versions,
                ) {
                    Ok(Output::Complete((statements, expressions))) => {
                        assert_eq!(v.len(), expressions.len());

                        self.complete &= true;

                        Ok(statements
                            .into_iter()
                            .chain(
                                v.into_iter()
                                    .zip(expressions)
                                    .map(|(v, e)| TypedStatement::Definition(v.into(), e)),
                            )
                            .collect())
                    }
                    Ok(Output::Incomplete((statements, expressions), delta_for_loop_versions)) => {
                        assert_eq!(v.len(), expressions.len());

                        self.complete = false;
                        self.for_loop_versions_after.extend(delta_for_loop_versions);

                        Ok(statements
                            .into_iter()
                            .chain(
                                v.into_iter()
                                    .zip(expressions)
                                    .map(|(v, e)| TypedStatement::Definition(v.into(), e)),
                            )
                            .collect())
                    }
                    Err(InlineError::Generic(decl, conc)) => {
                        Err(Error::Incompatible(decl.to_string(), conc.to_string()))
                    }
                    Err(InlineError::NonConstant(key, arguments, output_types)) => {
                        self.complete = false;

                        Ok(vec![TypedStatement::MultipleDefinition(
                            v,
                            TypedExpressionList::FunctionCall(key, arguments, output_types),
                        )])
                    }
                    Err(InlineError::Flat(embed, arguments, output_types)) => {
                        Ok(vec![TypedStatement::MultipleDefinition(
                            v,
                            TypedExpressionList::FunctionCall(
                                embed.key_in_module::<T>(&self.program.main).into(),
                                arguments,
                                output_types,
                            ),
                        )])
                    }
                }
            }
            TypedStatement::For(v, from, to, statements) => {
                let versions_before = self.for_loop_versions.pop().unwrap();

                match (from.as_inner(), to.as_inner()) {
                    (UExpressionInner::Value(from), UExpressionInner::Value(to)) => {
                        // println!("STORED VERSIONS: {:#?}", versions_before);
                        // println!("CURRENT VERSIONS: {:#?}", self.versions);

                        let mut out_statements = vec![];

                        // get a fresh set of versions for all variables to use as a starting point inside the loop
                        self.versions.values_mut().for_each(|v| *v = *v + 1);

                        // add this set of versions to the substitution, pointing to the versions before the loop
                        register(&mut self.substitutions, &self.versions, &versions_before);

                        // the versions after the loop are found by applying an offset of 2 to the versions before the loop
                        let versions_after = versions_before
                            .clone()
                            .into_iter()
                            .map(|(k, v)| (k, v + 2))
                            .collect();

                        let mut transformer = ShallowTransformer::with_versions(&mut self.versions);

                        for index in *from..*to {
                            let statements: Vec<TypedStatement<_>> =
                                std::iter::once(TypedStatement::Definition(
                                    v.clone().into(),
                                    UExpression::from(index as u32).into(),
                                ))
                                .chain(statements.clone().into_iter())
                                .map(|s| transformer.fold_statement(s))
                                .flatten()
                                .collect();

                            out_statements.extend(statements);
                        }

                        let backups = transformer.for_loop_backups;
                        let blocked = transformer.blocked;

                        // we know the final versions of the variables after full unrolling of the loop
                        // the versions after the loop need to point to these, so we add to the substitutions
                        register(&mut self.substitutions, &versions_after, &self.versions);

                        // we may have found new for loops when unrolling this one, which means new backed up versions
                        // we insert these in our backup list and update our cursor

                        self.for_loop_versions_after.extend(backups);

                        // if the ssa transform got blocked, the reduction is not complete
                        self.complete &= !blocked;

                        Ok(out_statements)
                    }
                    _ => {
                        self.complete = false;
                        self.for_loop_versions_after.push(versions_before);
                        Ok(vec![TypedStatement::For(v, from, to, statements)])
                    }
                }
            }
            s => fold_statement(self, s),
        };

        res.map(|res| self.statement_buffer.drain(..).chain(res).collect())
    }

    fn fold_boolean_expression(
        &mut self,
        e: BooleanExpression<'ast, T>,
    ) -> Result<BooleanExpression<'ast, T>, Self::Error> {
        match e {
            BooleanExpression::FunctionCall(key, arguments) => {
                self.fold_function_call(key, arguments, vec![Type::Boolean])
            }
            e => fold_boolean_expression(self, e),
        }
    }

    fn fold_uint_expression(
        &mut self,
        e: UExpression<'ast, T>,
    ) -> Result<UExpression<'ast, T>, Self::Error> {
        match e.as_inner() {
            UExpressionInner::FunctionCall(key, arguments) => {
                self.fold_function_call(key.clone(), arguments.clone(), vec![e.get_type()])
            }
            _ => fold_uint_expression(self, e),
        }
    }

    fn fold_field_expression(
        &mut self,
        e: FieldElementExpression<'ast, T>,
    ) -> Result<FieldElementExpression<'ast, T>, Self::Error> {
        match e {
            FieldElementExpression::FunctionCall(key, arguments) => {
                self.fold_function_call(key, arguments, vec![Type::FieldElement])
            }
            e => fold_field_expression(self, e),
        }
    }

    fn fold_array_expression(
        &mut self,
        e: ArrayExpression<'ast, T>,
    ) -> Result<ArrayExpression<'ast, T>, Self::Error> {
        match e.as_inner() {
            ArrayExpressionInner::FunctionCall(key, arguments) => {
                self.fold_function_call(key.clone(), arguments.clone(), vec![e.get_type()])
            }
            _ => fold_array_expression(self, e),
        }
    }

    fn fold_struct_expression(
        &mut self,
        e: StructExpression<'ast, T>,
    ) -> Result<StructExpression<'ast, T>, Self::Error> {
        match e.as_inner() {
            StructExpressionInner::FunctionCall(key, arguments) => {
                self.fold_function_call(key.clone(), arguments.clone(), vec![e.get_type()])
            }
            _ => fold_struct_expression(self, e),
        }
    }
}

pub fn reduce_program<'ast, T: Field>(
    p: TypedProgram<'ast, T>,
) -> Result<TypedProgram<'ast, T>, Error> {
    let mut p = p;

    let main_module = p.modules.get(&p.main).unwrap().clone();

    let (main_key, main_function) = main_module
        .functions
        .iter()
        .find(|(k, _)| k.id == "main")
        .unwrap()
        .clone();

    let main_function = match main_function {
        TypedFunctionSymbol::Here(f) => f.clone(),
        _ => unreachable!(),
    };

    let main_module = p.modules.get_mut(&p.main).unwrap();

    main_module.functions.extend(embeds_in_module(&p.main));

    match main_function.generics.len() {
        0 => {
            let main_function = reduce_function(main_function, GenericsAssignment::default(), &p)?;

            Ok(TypedProgram {
                main: p.main.clone(),
                modules: vec![(
                    p.main.clone(),
                    TypedModule {
                        functions: vec![(
                            main_key.clone(),
                            TypedFunctionSymbol::Here(main_function),
                        )]
                        .into_iter()
                        .chain(embeds_in_module(&p.main))
                        .collect(),
                    },
                )]
                .into_iter()
                .collect(),
            })
        }
        _ => Err(Error::GenericsInMain),
    }
}

fn reduce_function<'ast, T: Field>(
    f: TypedFunction<'ast, T>,
    generics: GenericsAssignment<'ast>,
    program: &TypedProgram<'ast, T>,
) -> Result<TypedFunction<'ast, T>, Error> {
    let mut versions = Versions::default();

    match ShallowTransformer::transform(f, &generics, &mut versions) {
        Output::Complete(f) => Ok(f),
        Output::Incomplete(new_f, new_for_loop_versions) => {
            let mut for_loop_versions = new_for_loop_versions;

            let mut f = new_f;

            let mut substitutions = Substitutions::default();

            loop {
                let mut reducer = Reducer::new(
                    &program,
                    &mut versions,
                    &mut substitutions,
                    for_loop_versions,
                );

                let statements: Vec<TypedStatement<'ast, T>> = f
                    .statements
                    .into_iter()
                    .map(|s| reducer.fold_statement(s))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .flatten()
                    .collect();

                assert!(reducer.for_loop_versions.is_empty());

                match reducer.complete {
                    true => {
                        let f = TypedFunction { statements, ..f };

                        substitutions = substitutions.canonicalize();

                        break Ok(Sub::new(&substitutions).fold_function(f));
                    }
                    false => {
                        let new_f = TypedFunction { statements, ..f };

                        for_loop_versions = reducer.for_loop_versions_after;

                        let new_f = Sub::new(&substitutions).fold_function(new_f);

                        f = Propagator::verbose().fold_function(new_f);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typed_absy::{
        ArrayExpressionInner, DeclarationFunctionKey, DeclarationType, DeclarationVariable,
        FieldElementExpression, Identifier, Type, TypedExpression, TypedExpressionList, UBitwidth,
        UExpression, UExpressionInner, Variable,
    };
    use typed_absy::types::Constant;
    use typed_absy::types::DeclarationSignature;
    use zokrates_field::Bn128Field;

    #[test]
    fn no_generics() {
        // def foo(field a) -> field:
        //      return a
        // def main(field a) -> field:
        //      u32 n = 42
        //      n = n
        //      a = a
        //      a = foo(a)
        //      n = n
        //      return a

        // expected:
        // def main(field a_0) -> field:
        //      u32 n_0 = 42
        //      n_1 = n_0
        //      a_1 = a_0
        //      # PUSH CALL to foo
        //          a_3 := a_1 // input binding
        //          #RETURN_AT_INDEX_0_0 := a_3
        //      # POP CALL
        //      a_2 = #RETURN_AT_INDEX_0_0
        //      n_2 = n_1
        //      return a_2

        let foo: TypedFunction<Bn128Field> = TypedFunction {
            generics: vec![],
            arguments: vec![DeclarationVariable::field_element("a").into()],
            statements: vec![TypedStatement::Return(vec![
                FieldElementExpression::Identifier("a".into()).into(),
            ])],
            signature: DeclarationSignature::new()
                .inputs(vec![DeclarationType::FieldElement])
                .outputs(vec![DeclarationType::FieldElement]),
        };

        let main: TypedFunction<Bn128Field> = TypedFunction {
            generics: vec![],
            arguments: vec![DeclarationVariable::field_element("a").into()],
            statements: vec![
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    TypedExpression::Uint(42u32.into()),
                ),
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    UExpressionInner::Identifier("n".into())
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Definition(
                    Variable::field_element("a").into(),
                    FieldElementExpression::Identifier("a".into()).into(),
                ),
                TypedStatement::MultipleDefinition(
                    vec![Variable::field_element("a").into()],
                    TypedExpressionList::FunctionCall(
                        DeclarationFunctionKey::with_location("main", "foo").signature(
                            DeclarationSignature::new()
                                .inputs(vec![DeclarationType::FieldElement])
                                .outputs(vec![DeclarationType::FieldElement]),
                        ),
                        vec![FieldElementExpression::Identifier("a".into()).into()],
                        vec![Type::FieldElement],
                    ),
                ),
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    UExpressionInner::Identifier("n".into())
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Return(vec![FieldElementExpression::Identifier("a".into()).into()]),
            ],
            signature: DeclarationSignature::new()
                .inputs(vec![DeclarationType::FieldElement])
                .outputs(vec![DeclarationType::FieldElement]),
        };

        let p = TypedProgram {
            main: "main".into(),
            modules: vec![(
                "main".into(),
                TypedModule {
                    functions: vec![
                        (
                            DeclarationFunctionKey::with_location("main", "foo").signature(
                                DeclarationSignature::new()
                                    .inputs(vec![DeclarationType::FieldElement])
                                    .outputs(vec![DeclarationType::FieldElement]),
                            ),
                            TypedFunctionSymbol::Here(foo),
                        ),
                        (
                            DeclarationFunctionKey::with_location("main", "main").signature(
                                DeclarationSignature::new()
                                    .inputs(vec![DeclarationType::FieldElement])
                                    .outputs(vec![DeclarationType::FieldElement]),
                            ),
                            TypedFunctionSymbol::Here(main),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                },
            )]
            .into_iter()
            .collect(),
        };

        let reduced = reduce_program(p);

        let expected_main = TypedFunction {
            generics: vec![],
            arguments: vec![DeclarationVariable::field_element("a").into()],
            statements: vec![
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    TypedExpression::Uint(42u32.into()),
                ),
                TypedStatement::Definition(
                    Variable::uint(Identifier::from("n").version(1), UBitwidth::B32).into(),
                    UExpressionInner::Identifier("n".into())
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Definition(
                    Variable::field_element(Identifier::from("a").version(1)).into(),
                    FieldElementExpression::Identifier("a".into()).into(),
                ),
                TypedStatement::PushCallLog(
                    DeclarationFunctionKey::with_location("main", "foo").signature(
                        DeclarationSignature::new()
                            .inputs(vec![DeclarationType::FieldElement])
                            .outputs(vec![DeclarationType::FieldElement]),
                    ),
                    GenericsAssignment::default(),
                ),
                TypedStatement::Definition(
                    Variable::field_element(Identifier::from("a").version(3)).into(),
                    FieldElementExpression::Identifier(Identifier::from("a").version(1)).into(),
                ),
                TypedStatement::Definition(
                    Variable::field_element(Identifier::from(CoreIdentifier::Call(0)).version(0))
                        .into(),
                    FieldElementExpression::Identifier(Identifier::from("a").version(3)).into(),
                ),
                TypedStatement::PopCallLog,
                TypedStatement::Definition(
                    Variable::field_element(Identifier::from("a").version(2)).into(),
                    FieldElementExpression::Identifier(
                        Identifier::from(CoreIdentifier::Call(0)).version(0),
                    )
                    .into(),
                ),
                TypedStatement::Definition(
                    Variable::uint(Identifier::from("n").version(2), UBitwidth::B32).into(),
                    UExpressionInner::Identifier(Identifier::from("n").version(1))
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Return(vec![FieldElementExpression::Identifier(
                    Identifier::from("a").version(2),
                )
                .into()]),
            ],
            signature: DeclarationSignature::new()
                .inputs(vec![DeclarationType::FieldElement])
                .outputs(vec![DeclarationType::FieldElement]),
        };

        let expected: TypedProgram<Bn128Field> = TypedProgram {
            main: "main".into(),
            modules: vec![(
                "main".into(),
                TypedModule {
                    functions: vec![(
                        DeclarationFunctionKey::with_location("main", "main").signature(
                            DeclarationSignature::new()
                                .inputs(vec![DeclarationType::FieldElement])
                                .outputs(vec![DeclarationType::FieldElement]),
                        ),
                        TypedFunctionSymbol::Here(expected_main),
                    )]
                    .into_iter()
                    .chain(embeds_in_module(&"main".into()))
                    .collect(),
                },
            )]
            .into_iter()
            .collect(),
        };

        assert_eq!(reduced.unwrap(), expected);
    }

    #[test]
    fn with_generics() {
        // def foo<K>(field[K] a) -> field[K]:
        //      return a
        // def main(field a) -> field:
        //      u32 n = 42
        //      n = n
        //      field[1] b = [42]
        //      b = foo(b)
        //      n = n
        //      return a

        // expected:
        // def main(field a_0) -> field:
        //      u32 n_0 = 42
        //      n_1 = n_0
        //      field[1] b_0 = [42]
        //      # PUSH CALL to foo::<1>
        //          a_0 = b_0
        //          K = 1
        //          #RETURN_AT_INDEX_0_0 := a_0
        //      # POP CALL
        //      b_1 = #RETURN_AT_INDEX_0_0
        //      n_2 = n_1
        //      return a_2

        let foo_signature = DeclarationSignature::new()
            .inputs(vec![DeclarationType::array(
                DeclarationType::FieldElement,
                Constant::Generic("K"),
            )])
            .outputs(vec![DeclarationType::array(
                DeclarationType::FieldElement,
                Constant::Generic("K"),
            )]);

        let foo: TypedFunction<Bn128Field> = TypedFunction {
            generics: vec!["K".into()],
            arguments: vec![DeclarationVariable::array(
                "a",
                DeclarationType::FieldElement,
                "K".into(),
            )
            .into()],
            statements: vec![TypedStatement::Return(vec![
                ArrayExpressionInner::Identifier("a".into())
                    .annotate(Type::FieldElement, 1u32)
                    .into(),
            ])],
            signature: foo_signature.clone(),
        };

        let main: TypedFunction<Bn128Field> = TypedFunction {
            generics: vec![],
            arguments: vec![DeclarationVariable::field_element("a").into()],
            statements: vec![
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    TypedExpression::Uint(42u32.into()),
                ),
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    UExpressionInner::Identifier("n".into())
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Definition(
                    Variable::array("b", Type::FieldElement, 1u32.into()).into(),
                    ArrayExpressionInner::Value(vec![
                        FieldElementExpression::Number(1.into()).into()
                    ])
                    .annotate(Type::FieldElement, 1u32)
                    .into(),
                ),
                TypedStatement::MultipleDefinition(
                    vec![Variable::array("b", Type::FieldElement, 1u32.into()).into()],
                    TypedExpressionList::FunctionCall(
                        DeclarationFunctionKey::with_location("main", "foo")
                            .signature(foo_signature.clone()),
                        vec![ArrayExpressionInner::Identifier("b".into())
                            .annotate(Type::FieldElement, 1u32)
                            .into()],
                        vec![Type::array(Type::FieldElement, 1u32)],
                    ),
                ),
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    UExpressionInner::Identifier("n".into())
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Return(vec![FieldElementExpression::Identifier("a".into()).into()]),
            ],
            signature: DeclarationSignature::new()
                .inputs(vec![DeclarationType::FieldElement])
                .outputs(vec![DeclarationType::FieldElement]),
        };

        let p = TypedProgram {
            main: "main".into(),
            modules: vec![(
                "main".into(),
                TypedModule {
                    functions: vec![
                        (
                            DeclarationFunctionKey::with_location("main", "foo")
                                .signature(foo_signature.clone()),
                            TypedFunctionSymbol::Here(foo),
                        ),
                        (
                            DeclarationFunctionKey::with_location("main", "main").signature(
                                DeclarationSignature::new()
                                    .inputs(vec![DeclarationType::FieldElement])
                                    .outputs(vec![DeclarationType::FieldElement]),
                            ),
                            TypedFunctionSymbol::Here(main),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                },
            )]
            .into_iter()
            .collect(),
        };

        let reduced = reduce_program(p);

        let expected_main = TypedFunction {
            generics: vec![],
            arguments: vec![DeclarationVariable::field_element("a").into()],
            statements: vec![
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    TypedExpression::Uint(42u32.into()),
                ),
                TypedStatement::Definition(
                    Variable::uint(Identifier::from("n").version(1), UBitwidth::B32).into(),
                    UExpressionInner::Identifier("n".into())
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Definition(
                    Variable::array("b", Type::FieldElement, 1u32.into()).into(),
                    ArrayExpressionInner::Value(vec![
                        FieldElementExpression::Number(1.into()).into()
                    ])
                    .annotate(Type::FieldElement, 1u32)
                    .into(),
                ),
                TypedStatement::PushCallLog(
                    DeclarationFunctionKey::with_location("main", "foo")
                        .signature(foo_signature.clone()),
                    GenericsAssignment(vec![("K", 1)].into_iter().collect()),
                ),
                TypedStatement::Definition(
                    Variable::array(
                        Identifier::from("a").version(1),
                        Type::FieldElement,
                        1u32.into(),
                    )
                    .into(),
                    ArrayExpressionInner::Identifier("b".into())
                        .annotate(Type::FieldElement, 1u32)
                        .into(),
                ),
                TypedStatement::Definition(
                    Variable::uint("K", UBitwidth::B32).into(),
                    TypedExpression::Uint(1u32.into()),
                ),
                TypedStatement::Definition(
                    Variable::array(
                        Identifier::from(CoreIdentifier::Call(0)).version(0),
                        Type::FieldElement,
                        1u32.into(),
                    )
                    .into(),
                    ArrayExpressionInner::Identifier(Identifier::from("a").version(1))
                        .annotate(Type::FieldElement, 1u32)
                        .into(),
                ),
                TypedStatement::PopCallLog,
                TypedStatement::Definition(
                    Variable::array(
                        Identifier::from("b").version(1),
                        Type::FieldElement,
                        1u32.into(),
                    )
                    .into(),
                    ArrayExpressionInner::Identifier(
                        Identifier::from(CoreIdentifier::Call(0)).version(0),
                    )
                    .annotate(Type::FieldElement, 1u32)
                    .into(),
                ),
                TypedStatement::Definition(
                    Variable::uint(Identifier::from("n").version(2), UBitwidth::B32).into(),
                    UExpressionInner::Identifier(Identifier::from("n").version(1))
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Return(vec![FieldElementExpression::Identifier("a".into()).into()]),
            ],
            signature: DeclarationSignature::new()
                .inputs(vec![DeclarationType::FieldElement])
                .outputs(vec![DeclarationType::FieldElement]),
        };

        let expected = TypedProgram {
            main: "main".into(),
            modules: vec![(
                "main".into(),
                TypedModule {
                    functions: vec![(
                        DeclarationFunctionKey::with_location("main", "main").signature(
                            DeclarationSignature::new()
                                .inputs(vec![DeclarationType::FieldElement])
                                .outputs(vec![DeclarationType::FieldElement]),
                        ),
                        TypedFunctionSymbol::Here(expected_main),
                    )]
                    .into_iter()
                    .chain(embeds_in_module(&"main".into()))
                    .collect(),
                },
            )]
            .into_iter()
            .collect(),
        };

        assert_eq!(reduced.unwrap(), expected);
    }

    #[test]
    fn with_generics_and_propagation() {
        // def foo<K>(field[K] a) -> field[K]:
        //      return a
        // def main(field a) -> field:
        //      u32 n = 2
        //      n = n
        //      field[n - 1] b = [42]
        //      b = foo(b)
        //      n = n
        //      return a

        // expected:
        // def main(field a_0) -> field:
        //      u32 n_0 = 2
        //      n_1 = 2
        //      field[1] b_0 = [42]
        //      # PUSH CALL to foo::<1>
        //          a_3 = b_0
        //          K = 1
        //          #RETURN_AT_INDEX_0_0 = a_3
        //      # POP CALL
        //      b_1 = #RETURN_AT_INDEX_0_0
        //      n_2 = 2
        //      return a_2

        let foo_signature = DeclarationSignature::new()
            .inputs(vec![DeclarationType::array(
                DeclarationType::FieldElement,
                Constant::Generic("K"),
            )])
            .outputs(vec![DeclarationType::array(
                DeclarationType::FieldElement,
                Constant::Generic("K"),
            )]);

        let foo: TypedFunction<Bn128Field> = TypedFunction {
            generics: vec!["K".into()],
            arguments: vec![DeclarationVariable::array(
                "a",
                DeclarationType::FieldElement,
                "K".into(),
            )
            .into()],
            statements: vec![TypedStatement::Return(vec![
                ArrayExpressionInner::Identifier("a".into())
                    .annotate(Type::FieldElement, 1u32)
                    .into(),
            ])],
            signature: foo_signature.clone(),
        };

        let main: TypedFunction<Bn128Field> = TypedFunction {
            generics: vec![],
            arguments: vec![DeclarationVariable::field_element("a").into()],
            statements: vec![
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    TypedExpression::Uint(2u32.into()),
                ),
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    UExpressionInner::Identifier("n".into())
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Definition(
                    Variable::array(
                        "b",
                        Type::FieldElement,
                        UExpressionInner::Sub(
                            box UExpressionInner::Identifier("n".into()).annotate(UBitwidth::B32),
                            box 1u32.into(),
                        )
                        .annotate(UBitwidth::B32),
                    )
                    .into(),
                    ArrayExpressionInner::Value(vec![
                        FieldElementExpression::Number(1.into()).into()
                    ])
                    .annotate(Type::FieldElement, 1u32)
                    .into(),
                ),
                TypedStatement::MultipleDefinition(
                    vec![Variable::array(
                        "b",
                        Type::FieldElement,
                        UExpressionInner::Sub(
                            box UExpressionInner::Identifier("n".into()).annotate(UBitwidth::B32),
                            box 1u32.into(),
                        )
                        .annotate(UBitwidth::B32),
                    )
                    .into()],
                    TypedExpressionList::FunctionCall(
                        DeclarationFunctionKey::with_location("main", "foo")
                            .signature(foo_signature.clone()),
                        vec![ArrayExpressionInner::Identifier("b".into())
                            .annotate(
                                Type::FieldElement,
                                UExpressionInner::Sub(
                                    box UExpressionInner::Identifier("n".into())
                                        .annotate(UBitwidth::B32),
                                    box 1u32.into(),
                                )
                                .annotate(UBitwidth::B32),
                            )
                            .into()],
                        vec![Type::array(
                            Type::FieldElement,
                            UExpressionInner::Sub(
                                box UExpressionInner::Identifier("n".into())
                                    .annotate(UBitwidth::B32),
                                box 1u32.into(),
                            )
                            .annotate(UBitwidth::B32),
                        )],
                    ),
                ),
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    UExpressionInner::Identifier("n".into())
                        .annotate(UBitwidth::B32)
                        .into(),
                ),
                TypedStatement::Return(vec![FieldElementExpression::Identifier("a".into()).into()]),
            ],
            signature: DeclarationSignature::new()
                .inputs(vec![DeclarationType::FieldElement])
                .outputs(vec![DeclarationType::FieldElement]),
        };

        let p = TypedProgram {
            main: "main".into(),
            modules: vec![(
                "main".into(),
                TypedModule {
                    functions: vec![
                        (
                            DeclarationFunctionKey::with_location("main", "foo")
                                .signature(foo_signature.clone()),
                            TypedFunctionSymbol::Here(foo),
                        ),
                        (
                            DeclarationFunctionKey::with_location("main", "main").signature(
                                DeclarationSignature::new()
                                    .inputs(vec![DeclarationType::FieldElement])
                                    .outputs(vec![DeclarationType::FieldElement]),
                            ),
                            TypedFunctionSymbol::Here(main),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                },
            )]
            .into_iter()
            .collect(),
        };

        let reduced = reduce_program(p);

        let expected_main = TypedFunction {
            generics: vec![],
            arguments: vec![DeclarationVariable::field_element("a").into()],
            statements: vec![
                TypedStatement::Definition(
                    Variable::uint("n", UBitwidth::B32).into(),
                    TypedExpression::Uint(2u32.into()),
                ),
                TypedStatement::Definition(
                    Variable::uint(Identifier::from("n").version(1), UBitwidth::B32).into(),
                    TypedExpression::Uint(2u32.into()),
                ),
                TypedStatement::Definition(
                    Variable::array("b", Type::FieldElement, 1u32.into()).into(),
                    ArrayExpressionInner::Value(vec![
                        FieldElementExpression::Number(1.into()).into()
                    ])
                    .annotate(Type::FieldElement, 1u32)
                    .into(),
                ),
                TypedStatement::PushCallLog(
                    DeclarationFunctionKey::with_location("main", "foo")
                        .signature(foo_signature.clone()),
                    GenericsAssignment(vec![("K", 1)].into_iter().collect()),
                ),
                TypedStatement::Definition(
                    Variable::array(
                        Identifier::from("a").version(1),
                        Type::FieldElement,
                        1u32.into(),
                    )
                    .into(),
                    ArrayExpressionInner::Value(vec![
                        FieldElementExpression::Number(1.into()).into()
                    ])
                    .annotate(Type::FieldElement, 1u32)
                    .into(),
                ),
                TypedStatement::Definition(
                    Variable::uint("K", UBitwidth::B32).into(),
                    UExpression::from(1u32).into(),
                ),
                TypedStatement::Definition(
                    Variable::array(
                        Identifier::from(CoreIdentifier::Call(0)).version(0),
                        Type::FieldElement,
                        1u32.into(),
                    )
                    .into(),
                    ArrayExpressionInner::Identifier(Identifier::from("a").version(1))
                        .annotate(Type::FieldElement, 1u32)
                        .into(),
                ),
                TypedStatement::PopCallLog,
                TypedStatement::Definition(
                    Variable::array(
                        Identifier::from("b").version(1),
                        Type::FieldElement,
                        1u32.into(),
                    )
                    .into(),
                    ArrayExpressionInner::Identifier(
                        Identifier::from(CoreIdentifier::Call(0)).version(0),
                    )
                    .annotate(Type::FieldElement, 1u32)
                    .into(),
                ),
                TypedStatement::Definition(
                    Variable::uint(Identifier::from("n").version(2), UBitwidth::B32).into(),
                    TypedExpression::Uint(2u32.into()),
                ),
                TypedStatement::Return(vec![FieldElementExpression::Identifier("a".into()).into()]),
            ],
            signature: DeclarationSignature::new()
                .inputs(vec![DeclarationType::FieldElement])
                .outputs(vec![DeclarationType::FieldElement]),
        };

        let expected = TypedProgram {
            main: "main".into(),
            modules: vec![(
                "main".into(),
                TypedModule {
                    functions: vec![(
                        DeclarationFunctionKey::with_location("main", "main").signature(
                            DeclarationSignature::new()
                                .inputs(vec![DeclarationType::FieldElement])
                                .outputs(vec![DeclarationType::FieldElement]),
                        ),
                        TypedFunctionSymbol::Here(expected_main),
                    )]
                    .into_iter()
                    .chain(embeds_in_module(&"main".into()))
                    .collect(),
                },
            )]
            .into_iter()
            .collect(),
        };

        assert_eq!(reduced.unwrap(), expected);
    }

    // #[test]
    // fn call_in_call() {
    //     // we use a global ssa counter, hence reusing variable names in called functions
    //     // leads to counter increase

    //     // def bar<K>(field[K] a) -> field[K]:
    //     //      return a

    //     // def foo<K>(field[K] a) -> field[K]:
    //     //      field[K] ret = bar([...a, 0])[0..K]
    //     //      return ret

    //     // def main():
    //     //      field[1] b = foo([1])
    //     //      return

    //     // expected:
    //     // def main():
    //     //      # PUSH CALL to foo::<1> with a_0 := [1]
    //     //      # PUSH CALL to bar::<2> with a_0 := [...a, 0]
    //     //      # POP CALL with field[1] ~bar_0 := a_0
    //     //      field[1] ret = ~bar_0[0..1]
    //     //      # POP CALL with field[1] b_0 := ret
    //     //      return

    //     let foo_signature = DeclarationSignature::new()
    //         .inputs(vec![DeclarationType::array(
    //             DeclarationType::FieldElement,
    //             Constant::Generic("K"),
    //         )])
    //         .outputs(vec![DeclarationType::array(
    //             DeclarationType::FieldElement,
    //             Constant::Generic("K"),
    //         )]);

    //     let foo: TypedFunction<Bn128Field> = TypedFunction {
    //         generics: vec!["K".into()],
    //         arguments: vec![DeclarationVariable::array(
    //             "a",
    //             DeclarationType::FieldElement,
    //             "K".into(),
    //         )
    //         .into()],
    //         statements: vec![TypedStatement::Return(vec![
    //             ArrayExpressionInner::Identifier("a".into())
    //                 .annotate(
    //                     Type::FieldElement,
    //                     UExpressionInner::Identifier("K".into()).annotate(UBitwidth::B32),
    //                 )
    //                 .into(),
    //         ])],
    //         signature: foo_signature.clone(),
    //     };

    //     let bar_signature = DeclarationSignature::new()
    //         .inputs(vec![DeclarationType::array(
    //             DeclarationType::FieldElement,
    //             Constant::Generic("K"),
    //         )])
    //         .outputs(vec![DeclarationType::array(
    //             DeclarationType::FieldElement,
    //             Constant::Generic("K"),
    //         )]);

    //     let bar: TypedFunction<Bn128Field> = TypedFunction {
    //         generics: vec!["K".into()],
    //         arguments: vec![DeclarationVariable::array(
    //             "a",
    //             DeclarationType::FieldElement,
    //             "K".into(),
    //         )
    //         .into()],
    //         statements: vec![TypedStatement::Return(vec![
    //             ArrayExpressionInner::Identifier("a".into())
    //                 .annotate(
    //                     Type::FieldElement,
    //                     UExpressionInner::Identifier("K".into()).annotate(UBitwidth::B32),
    //                 )
    //                 .into(),
    //         ])],
    //         signature: foo_signature.clone(),
    //     };

    //     let main: TypedFunction<Bn128Field> = TypedFunction {
    //         generics: vec![],
    //         arguments: vec![DeclarationVariable::field_element("a").into()],
    //         statements: vec![
    //             TypedStatement::MultipleDefinition(
    //                 vec![Variable::array("b", Type::FieldElement, 1u32.into()).into()],
    //                 TypedExpressionList::FunctionCall(
    //                     DeclarationFunctionKey::with_location(module_id.clone(), "foo").signature(foo_signature.clone()),
    //                     vec![ArrayExpressionInner::Identifier("b".into())
    //                         .annotate(Type::FieldElement, 1u32)
    //                         .into()],
    //                     vec![Type::array(Type::FieldElement, 1u32)],
    //                 ),
    //             ),
    //             TypedStatement::Return(vec![]),
    //         ],
    //         signature: DeclarationSignature::new(),
    //     };

    //     let p = TypedProgram {
    //         main: "main".into(),
    //         modules: vec![(
    //             "main".into(),
    //             TypedModule {
    //                 functions: vec![
    //                     (
    //                         DeclarationFunctionKey::with_location(module_id.clone(), "foo").signature(foo_signature.clone()),
    //                         TypedFunctionSymbol::Here(foo),
    //                     ),
    //                     (
    //                         DeclarationFunctionKey::with_location(module_id.clone(), "main"),
    //                         TypedFunctionSymbol::Here(main),
    //                     ),
    //                 ]
    //                 .into_iter()
    //                 .collect(),
    //             },
    //         )]
    //         .into_iter()
    //         .collect(),
    //     };

    //     let reduced = reduce_program(p);

    //     let expected_main = TypedFunction {
    //         generics: vec![],
    //         arguments: vec![DeclarationVariable::field_element("a").into()],
    //         statements: vec![
    //             TypedStatement::Definition(
    //                 Variable::uint("n", UBitwidth::B32).into(),
    //                 TypedExpression::Uint(42u32.into()),
    //             ),
    //             TypedStatement::Definition(
    //                 Variable::uint(Identifier::from("n").version(1), UBitwidth::B32).into(),
    //                 UExpressionInner::Identifier("n".into())
    //                     .annotate(UBitwidth::B32)
    //                     .into(),
    //             ),
    //             TypedStatement::Definition(
    //                 Variable::array("b", Type::FieldElement, 1u32.into()).into(),
    //                 ArrayExpressionInner::Value(vec![
    //                     FieldElementExpression::Number(1.into()).into()
    //                 ])
    //                 .annotate(Type::FieldElement, 1u32)
    //                 .into(),
    //             ),
    //             TypedStatement::PushCallLog(
    //                 "main".into(),
    //                 DeclarationFunctionKey::with_location(module_id.clone(), "foo").signature(foo_signature.clone()),
    //                 GenericsAssignment(vec![("K", 1)].into_iter().collect()),
    //                 vec![(
    //                     ConcreteVariable::array("a", ConcreteType::FieldElement, 1).into(),
    //                     ArrayExpressionInner::Identifier("b".into())
    //                         .annotate(Type::FieldElement, 1u32)
    //                         .into(),
    //                 )],
    //             ),
    //             TypedStatement::Definition(
    //                 Variable::uint("K", UBitwidth::B32).into(),
    //                 UExpression::from(1u32).into(),
    //             ),
    //             TypedStatement::PopCallLog(vec![(
    //                 ConcreteVariable::array(
    //                     Identifier::from("b").version(1),
    //                     ConcreteType::FieldElement,
    //                     1,
    //                 ),
    //                 ArrayExpressionInner::Identifier("a".into())
    //                     .annotate(Type::FieldElement, 1u32)
    //                     .into(),
    //             )]),
    //             TypedStatement::Definition(
    //                 Variable::uint(Identifier::from("n").version(2), UBitwidth::B32).into(),
    //                 UExpressionInner::Identifier(Identifier::from("n").version(1))
    //                     .annotate(UBitwidth::B32)
    //                     .into(),
    //             ),
    //             TypedStatement::Return(vec![FieldElementExpression::Identifier("a".into()).into()]),
    //         ],
    //         signature: DeclarationSignature::new()
    //             .inputs(vec![DeclarationType::FieldElement])
    //             .outputs(vec![DeclarationType::FieldElement]),
    //     };

    //     let expected = TypedProgram {
    //         main: "main".into(),
    //         modules: vec![(
    //             "main".into(),
    //             TypedModule {
    //                 functions: vec![(
    //                     DeclarationFunctionKey::with_location(module_id.clone(), "main").signature(
    //                         DeclarationSignature::new()
    //                             .inputs(vec![DeclarationType::FieldElement])
    //                             .outputs(vec![DeclarationType::FieldElement]),
    //                     ),
    //                     TypedFunctionSymbol::Here(expected_main),
    //                 )]
    //                 .into_iter()
    //                 .collect(),
    //             },
    //         )]
    //         .into_iter()
    //         .collect(),
    //     };

    //     println!("{}", reduced.clone().unwrap());
    //     println!("{}", expected);

    //     assert_eq!(reduced.unwrap(), expected);
    // }

    #[test]
    fn incompatible() {
        // def foo<K>(field[K] a) -> field[K]:
        //      return a
        // def main():
        //      field[1] b = foo([])
        //      return

        // expected:
        // Error: Incompatible

        use typed_absy::types::{ConcreteFunctionKey, ConcreteSignature, ConcreteType};

        let foo_signature = DeclarationSignature::new()
            .inputs(vec![DeclarationType::array(
                DeclarationType::FieldElement,
                Constant::Generic("K"),
            )])
            .outputs(vec![DeclarationType::array(
                DeclarationType::FieldElement,
                Constant::Generic("K"),
            )]);

        let foo: TypedFunction<Bn128Field> = TypedFunction {
            generics: vec!["K".into()],
            arguments: vec![DeclarationVariable::array(
                "a",
                DeclarationType::FieldElement,
                "K".into(),
            )
            .into()],
            statements: vec![TypedStatement::Return(vec![
                ArrayExpressionInner::Identifier("a".into())
                    .annotate(Type::FieldElement, 1u32)
                    .into(),
            ])],
            signature: foo_signature.clone(),
        };

        let main: TypedFunction<Bn128Field> = TypedFunction {
            generics: vec![],
            arguments: vec![],
            statements: vec![
                TypedStatement::MultipleDefinition(
                    vec![Variable::array("b", Type::FieldElement, 1u32.into()).into()],
                    TypedExpressionList::FunctionCall(
                        DeclarationFunctionKey::with_location("main", "foo")
                            .signature(foo_signature.clone()),
                        vec![ArrayExpressionInner::Value(vec![])
                            .annotate(Type::FieldElement, 0u32)
                            .into()],
                        vec![Type::array(Type::FieldElement, 1u32)],
                    ),
                ),
                TypedStatement::Return(vec![]),
            ],
            signature: DeclarationSignature::new().inputs(vec![]).outputs(vec![]),
        };

        let p = TypedProgram {
            main: "main".into(),
            modules: vec![(
                "main".into(),
                TypedModule {
                    functions: vec![
                        (
                            DeclarationFunctionKey::with_location("main", "foo")
                                .signature(foo_signature.clone()),
                            TypedFunctionSymbol::Here(foo),
                        ),
                        (
                            DeclarationFunctionKey::with_location("main", "main").signature(
                                DeclarationSignature::new().inputs(vec![]).outputs(vec![]),
                            ),
                            TypedFunctionSymbol::Here(main),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                },
            )]
            .into_iter()
            .collect(),
        };

        let reduced = reduce_program(p);

        assert_eq!(
            reduced,
            Err(Error::Incompatible(
                DeclarationFunctionKey::with_location("main", "foo")
                    .signature(foo_signature.clone())
                    .to_string(),
                ConcreteFunctionKey::with_location("main", "foo")
                    .signature(
                        ConcreteSignature::new()
                            .inputs(vec![ConcreteType::array(
                                ConcreteType::FieldElement,
                                0usize
                            )])
                            .outputs(vec![ConcreteType::array(
                                ConcreteType::FieldElement,
                                1usize
                            )])
                    )
                    .to_string()
            ))
        );
    }
}