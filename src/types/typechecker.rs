//! typechecker.rs - Defines the type inference pass used by the compiler.
//! This pass comes after name resolution and is followed by the lifetime inference.
//!
//! This pass traverses over the ast, filling out the (typ: Option<Type>) field of each node.
//! When this pass is finished, all such fields are guarenteed to be filled out. The formatting
//! of this file begins with helper functions for type inference at the type, and ends with
//! the actual AST pass defined in the `Inferable` trait. Note that this AST pass starts
//! in the first module, and whenever it finds a variable using a definition that hasn't yet
//! been typechecked, it delves into that definition to typecheck it. This means any variables
//! that are unused are not typechecked by default.
//!
//! This uses algorithm j extended with let polymorphism and multi-parameter
//! typeclasses (traits) with a very limited form of functional dependencies.
//! For generalization this uses let binding levels to determine if types escape
//! the current binding and should thus not be generalized.
//!
//! Most of this file is translated from: https://github.com/jfecher/algorithm-j
//! That repository may be a good starting place for those new to type inference.
//! For those already familiar with type inference or more interested in ante's
//! internals, the reccomended starting place while reading this file is the
//! `Inferable` trait and its impls for each node. From there, you can see what
//! type inference does for each node type and inspect any helpers that are used.
//!
//! Note that as a result of type inference, the following Optional fields in the
//! Ast will be filled out:
//! - `typ: Option<Type>` for all nodes,
//! - `trait_binding: Option<TraitBindingId>` for `ast::Variable`s,
//! - `decision_tree: Option<DecisionTree>` for `ast::Match`s
use crate::cache::{DefinitionInfoId, DefinitionKind, ModuleCache, TraitInfoId};
use crate::cache::{ImplScopeId, VariableId};
use crate::error::location::{Locatable, Location};
use crate::error::{get_error_count, ErrorMessage};
use crate::lexer::token::IntegerKind;
use crate::parser::ast::{self, ClosureEnvironment};
use crate::types::traits::{RequiredTrait, TraitConstraint, TraitConstraints};
use crate::types::typed::Typed;
use crate::types::{
    pattern, traitchecker, FunctionType, LetBindingLevel, PrimitiveType, Type, Type::*, TypeBinding, TypeBinding::*,
    TypeInfo, TypeVariableId, INITIAL_LEVEL, PAIR_TYPE, STRING_TYPE,
};
use crate::util::*;

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::traits::{Callsite, ConstraintSignature, TraitConstraintId};
use super::GeneralizedType;

/// The current LetBindingLevel we are at.
/// This increases by 1 whenever we enter the rhs of a `ast::Definition` and decreases
/// by 1 whenever we exit this rhs. This helps keep track of which scope type variables
/// arose from and whether they should be generalized or not. See
/// http://okmij.org/ftp/ML/generalization.html for more information on let binding levels.
pub static CURRENT_LEVEL: AtomicUsize = AtomicUsize::new(INITIAL_LEVEL);

/// A sparse set of type bindings, used by try_unify
pub type TypeBindings = HashMap<TypeVariableId, Type>;

/// The result of `try_unify`: either a set of type bindings to perform,
/// or an error message of which types failed to unify.
pub type UnificationResult<'c> = Result<UnificationBindings, ErrorMessage<'c>>;

type LevelBindings = Vec<(TypeVariableId, LetBindingLevel)>;

#[derive(Debug, Clone)]
pub struct UnificationBindings {
    pub bindings: TypeBindings,
    level_bindings: LevelBindings,
}

impl UnificationBindings {
    pub fn empty() -> UnificationBindings {
        UnificationBindings { bindings: HashMap::new(), level_bindings: vec![] }
    }

    pub fn new(bindings: TypeBindings, level_bindings: LevelBindings) -> UnificationBindings {
        UnificationBindings { bindings, level_bindings }
    }

    pub fn perform(self, cache: &mut ModuleCache) {
        perform_type_bindings(self.bindings, cache);

        for (id, level) in self.level_bindings {
            match &cache.type_bindings[id.0] {
                Bound(_) => (), // The binding changed from under us. Is this an issue?
                Unbound(original_level, kind) => {
                    let min_level = std::cmp::min(level, *original_level);
                    cache.type_bindings[id.0] = Unbound(min_level, kind.clone());
                },
            }
        }
    }

    pub fn extend(&mut self, mut other: UnificationBindings) {
        self.bindings.extend(other.bindings);
        self.level_bindings.append(&mut other.level_bindings);
    }
}

/// Convert a TypeApplication(UserDefinedType(id), args) into the set of TypeBindings
/// so that each mapping in the bindings is in the form `var -> arg` where each variable
/// was one of the variables given in the definition of the user-defined-type:
/// `type Foo var1 var2 ... varN = ...` and each `arg` corresponds to the generic argument
/// of the type somewhere in the program, e.g: `foo : Foo arg1 arg2 ... argN`
pub fn type_application_bindings<'c>(info: &TypeInfo<'c>, typeargs: &[Type]) -> TypeBindings {
    info.args.iter().copied().zip(typeargs.iter().cloned()).collect()
}

/// Replace any typevars found in typevars_to_replace with the
/// associated value in the same table, leave them otherwise
fn replace_typevars<'c>(
    typ: &Type, typevars_to_replace: &HashMap<TypeVariableId, TypeVariableId>, cache: &ModuleCache<'c>,
) -> Type {
    let typevars_to_replace = typevars_to_replace.iter().map(|(key, id)| (*key, TypeVariable(*id))).collect();

    bind_typevars(typ, &typevars_to_replace, cache)
}

/// Return a new type with all typevars found in the given type
/// replaced with fresh ones, along with the type bindings used.
///
/// Note that unlike `generalize(typ).instantiate(..)`, this will
/// replace all type variables rather than only type variables
/// that have not originated from an outer scope.
pub fn replace_all_typevars<'c>(types: &[Type], cache: &mut ModuleCache<'c>) -> (Vec<Type>, TypeBindings) {
    let mut bindings = HashMap::new();
    let types = fmap(types, |typ| replace_all_typevars_with_bindings(typ, &mut bindings, cache));
    (types, bindings)
}

/// Replace all type variables in the given type, using new_bindings
/// to lookup what each variable should be bound to, inserting a
/// fresh type variable into new_bindings if that type variable was not present.
pub fn replace_all_typevars_with_bindings<'c>(
    typ: &Type, new_bindings: &mut TypeBindings, cache: &mut ModuleCache<'c>,
) -> Type {
    match typ {
        Primitive(p) => Primitive(*p),

        TypeVariable(id) => replace_typevar_with_binding(*id, new_bindings, TypeVariable, cache),

        Function(function) => {
            let parameters = fmap(&function.parameters, |parameter| {
                replace_all_typevars_with_bindings(parameter, new_bindings, cache)
            });
            let return_type = Box::new(replace_all_typevars_with_bindings(&function.return_type, new_bindings, cache));
            let environment = Box::new(replace_all_typevars_with_bindings(&function.environment, new_bindings, cache));
            let is_varargs = function.is_varargs;
            Function(FunctionType { parameters, return_type, environment, is_varargs })
        },
        UserDefined(id) => UserDefined(*id),

        // We must recurse on the lifetime variable since they are unified as normal type variables
        Ref(lifetime) => match replace_typevar_with_binding(*lifetime, new_bindings, Ref, cache) {
            TypeVariable(new_lifetime) => Ref(new_lifetime),
            Ref(new_lifetime) => Ref(new_lifetime),
            _ => unreachable!("Bound Ref lifetime to non-lifetime type"),
        },

        TypeApplication(typ, args) => {
            let typ = replace_all_typevars_with_bindings(typ, new_bindings, cache);
            let args = fmap(args, |arg| replace_all_typevars_with_bindings(arg, new_bindings, cache));
            TypeApplication(Box::new(typ), args)
        },
    }
}

/// If the given TypeVariableId is unbound then return the matching binding in new_bindings.
/// If there is no binding found, instantiate a new type variable and use that.
///
/// `default` should be either TypeVariable or Ref and controls which kind of type gets
/// created that wraps the newly-instantiated TypeVariableId if one is made.
fn replace_typevar_with_binding<'c>(
    id: TypeVariableId, new_bindings: &mut TypeBindings, default: fn(TypeVariableId) -> Type,
    cache: &mut ModuleCache<'c>,
) -> Type {
    if let Bound(typ) = &cache.type_bindings[id.0] {
        replace_all_typevars_with_bindings(&typ.clone(), new_bindings, cache)
    } else if let Some(var) = new_bindings.get(&id) {
        var.clone()
    } else {
        let new_typevar = next_type_variable_id(cache);
        new_bindings.insert(id, default(new_typevar));
        default(new_typevar)
    }
}

/// Replace any typevars found with the given type bindings
///
/// Compared to `replace_all_typevars_with_bindings`, this function does not instantiate
/// unbound type variables that were not in type_bindings. Thus if type_bindings is empty,
/// this function will just clone the original Type.
pub fn bind_typevars<'c>(typ: &Type, type_bindings: &TypeBindings, cache: &ModuleCache<'c>) -> Type {
    match typ {
        Primitive(p) => Primitive(*p),

        TypeVariable(id) => bind_typevar(*id, type_bindings, TypeVariable, cache),

        Function(function) => {
            let parameters = fmap(&function.parameters, |parameter| bind_typevars(parameter, type_bindings, cache));
            let return_type = Box::new(bind_typevars(&function.return_type, type_bindings, cache));
            let environment = Box::new(bind_typevars(&function.environment, type_bindings, cache));
            let is_varargs = function.is_varargs;
            Function(FunctionType { parameters, return_type, environment, is_varargs })
        },
        UserDefined(id) => UserDefined(*id),

        Ref(lifetime) => match bind_typevar(*lifetime, type_bindings, Ref, cache) {
            TypeVariable(new_lifetime) => Ref(new_lifetime),
            Ref(new_lifetime) => Ref(new_lifetime),
            _ => unreachable!("Bound Ref lifetime to non-lifetime type"),
        },

        TypeApplication(typ, args) => {
            let typ = bind_typevars(typ, type_bindings, cache);
            let args = fmap(args, |arg| bind_typevars(arg, type_bindings, cache));
            TypeApplication(Box::new(typ), args)
        },
    }
}

/// Helper for bind_typevars which binds a single TypeVariableId if it is Unbound
/// and it is found in the type_bindings. If a type_binding wasn't found, a
/// default TypeVariable or Ref is constructed by passing the relevant constructor to `default`.
fn bind_typevar<'c>(
    id: TypeVariableId, type_bindings: &TypeBindings, default: fn(TypeVariableId) -> Type, cache: &ModuleCache<'c>,
) -> Type {
    // TODO: This ordering of checking type_bindings first is important.
    // There seems to be an issue currently where forall-bound variables
    // can be bound in the cache, so checking the cache for bindings first
    // can prevent us from instantiating these variables.
    match type_bindings.get(&id) {
        Some(binding) => binding.clone(),
        None => {
            if let Bound(typ) = &cache.type_bindings[id.0] {
                bind_typevars(&typ.clone(), type_bindings, cache)
            } else {
                default(id)
            }
        },
    }
}

/// Recurse on typ, returning true if it contains any of the TypeVariableIds
/// contained within list.
pub fn contains_any_typevars_from_list<'c>(typ: &Type, list: &[TypeVariableId], cache: &ModuleCache<'c>) -> bool {
    match typ {
        Primitive(_) => false,
        UserDefined(_) => false,

        TypeVariable(id) => type_variable_contains_any_typevars_from_list(*id, list, cache),

        Function(function) => {
            function.parameters.iter().any(|parameter| contains_any_typevars_from_list(parameter, list, cache))
                || contains_any_typevars_from_list(&function.return_type, list, cache)
                || contains_any_typevars_from_list(&function.environment, list, cache)
        },

        Ref(lifetime) => type_variable_contains_any_typevars_from_list(*lifetime, list, cache),

        TypeApplication(typ, args) => {
            contains_any_typevars_from_list(typ, list, cache)
                || args.iter().any(|arg| contains_any_typevars_from_list(arg, list, cache))
        },
    }
}

fn type_variable_contains_any_typevars_from_list<'c>(
    id: TypeVariableId, list: &[TypeVariableId], cache: &ModuleCache<'c>,
) -> bool {
    if let Bound(typ) = &cache.type_bindings[id.0] {
        contains_any_typevars_from_list(typ, list, cache)
    } else {
        list.contains(&id)
    }
}

/// Helper function for getting the next type variable at the current level
fn next_type_variable_id(cache: &mut ModuleCache) -> TypeVariableId {
    let level = LetBindingLevel(CURRENT_LEVEL.load(Ordering::SeqCst));
    cache.next_type_variable_id(level)
}

fn next_type_variable(cache: &mut ModuleCache) -> Type {
    let level = LetBindingLevel(CURRENT_LEVEL.load(Ordering::SeqCst));
    cache.next_type_variable(level)
}

fn to_trait_constraints(
    id: DefinitionInfoId, scope: ImplScopeId, callsite: VariableId, cache: &mut ModuleCache,
) -> TraitConstraints {
    let info = &cache.definition_infos[id.0];
    let current_constraint_id = &mut cache.current_trait_constraint_id;

    let mut traits = fmap(&info.required_traits, |required_trait| {
        let id = current_constraint_id.next();
        required_trait.as_constraint(scope, callsite, id)
    });

    // If this definition is from a trait, we must add the initial constraint directly
    if let Some((trait_id, args)) = &info.trait_info {
        let id = current_constraint_id.next();

        traits.push(TraitConstraint {
            required: RequiredTrait {
                signature: ConstraintSignature { trait_id: *trait_id, args: args.clone(), id },
                callsite: Callsite::Direct(callsite),
            },
            scope,
        });
    }

    traits
}

/// specializes the polytype s by copying the term and replacing the
/// bound type variables consistently by new monotype variables.
/// Returns the type bindings used to instantiate the type.
///
/// E.g.   instantiate (forall a b. a -> b -> a) = c -> d -> c
///
/// This will also instantiate each given trait constraint, replacing
/// each free typevar of the constraint's argument types.
impl GeneralizedType {
    pub fn instantiate<'b>(
        &self, mut constraints: TraitConstraints, cache: &mut ModuleCache<'b>,
    ) -> (Type, TraitConstraints, TypeBindings) {
        // Note that the returned type is no longer a PolyType,
        // this means it is now monomorphic and not forall-quantified
        match self {
            GeneralizedType::MonoType(typ) => (typ.clone(), constraints, HashMap::new()),
            GeneralizedType::PolyType(typevars, typ) => {
                // Must replace all typevars in typ and the required_traits list with new ones
                let mut typevars_to_replace = HashMap::new();
                for var in typevars.iter().copied() {
                    typevars_to_replace.insert(var, next_type_variable_id(cache));
                }
                let typ = replace_typevars(typ, &typevars_to_replace, cache);

                for var in find_all_typevars_in_traits(&constraints, cache).iter().copied() {
                    typevars_to_replace.entry(var).or_insert_with(|| next_type_variable_id(cache));
                }

                for constraint in constraints.iter_mut() {
                    for typ in constraint.args_mut() {
                        *typ = replace_typevars(typ, &typevars_to_replace, cache);
                    }
                }

                let type_bindings = typevars_to_replace.into_iter().map(|(k, v)| (k, TypeVariable(v))).collect();
                (typ, constraints, type_bindings)
            },
        }
    }
}

/// Similar to instantiate but uses an explicitly passed map to map
/// the old type variables to. This version is used during trait impl
/// type inference to ensure all definitions in the trait impl are
/// mapped to the same typevars, rather than each definition instantiated
/// separately as is normal.
///
/// This version is also different in that it also replaces the type variables
/// of monotypes.
fn instantiate_impl_with_bindings<'b>(
    typ: &GeneralizedType, bindings: &mut TypeBindings, cache: &mut ModuleCache<'b>,
) -> GeneralizedType {
    use GeneralizedType::*;
    match typ {
        MonoType(typ) => MonoType(replace_all_typevars_with_bindings(typ, bindings, cache)),
        PolyType(_, typ) => {
            // unreachable!("Impl already inferred to have polymorphic typ, {}", typ.debug(cache)),
            MonoType(replace_all_typevars_with_bindings(typ, bindings, cache))
        },
    }
}

fn find_binding<'b>(id: TypeVariableId, map: &UnificationBindings, cache: &ModuleCache<'b>) -> TypeBinding {
    match &cache.type_bindings[id.0] {
        Bound(typ) => Bound(typ.clone()),
        Unbound(level, kind) => match map.bindings.get(&id) {
            Some(typ) => Bound(typ.clone()),
            None => Unbound(*level, kind.clone()),
        },
    }
}

struct OccursResult {
    occurs: bool,
    level_bindings: LevelBindings,
}

impl OccursResult {
    fn does_not_occur() -> OccursResult {
        OccursResult { occurs: false, level_bindings: vec![] }
    }

    fn new(occurs: bool, level_bindings: LevelBindings) -> OccursResult {
        OccursResult { occurs, level_bindings }
    }

    fn then(mut self, mut f: impl FnMut() -> OccursResult) -> OccursResult {
        if !self.occurs {
            let mut other = f();
            self.occurs = other.occurs;
            self.level_bindings.append(&mut other.level_bindings);
        }
        self
    }

    fn then_all(mut self, types: &[Type], mut f: impl FnMut(&Type) -> OccursResult) -> OccursResult {
        if !self.occurs {
            for typ in types {
                let mut other = f(typ);
                self.occurs = other.occurs;
                self.level_bindings.append(&mut other.level_bindings);
                if self.occurs {
                    return self;
                }
            }
        }
        self
    }
}

/// Can a monomorphic TypeVariable(id) be found inside this type?
/// This will mutate any typevars found to increase their LetBindingLevel.
/// Doing so increases the lifetime of the typevariable and lets us keep
/// track of which type variables to generalize later on. It also means
/// that occurs should only be called during unification however.
fn occurs<'b>(
    id: TypeVariableId, level: LetBindingLevel, typ: &Type, bindings: &mut UnificationBindings,
    cache: &mut ModuleCache<'b>,
) -> OccursResult {
    match typ {
        Primitive(_) => OccursResult::does_not_occur(),
        UserDefined(_) => OccursResult::does_not_occur(),

        TypeVariable(var_id) => typevars_match(id, level, *var_id, bindings, cache),
        Function(function) => occurs(id, level, &function.return_type, bindings, cache)
            .then(|| occurs(id, level, &function.environment, bindings, cache))
            .then_all(&function.parameters, |param| occurs(id, level, param, bindings, cache)),
        TypeApplication(typ, args) => {
            occurs(id, level, typ, bindings, cache).then_all(args, |arg| occurs(id, level, arg, bindings, cache))
        },
        Ref(lifetime) => typevars_match(id, level, *lifetime, bindings, cache),
    }
}

/// Helper function for the `occurs` check.
///
/// Recurse within `haystack` to try to find an Unbound typevar and check if it
/// has the same Id as the needle TypeVariableId.
fn typevars_match<'c>(
    needle: TypeVariableId, level: LetBindingLevel, haystack: TypeVariableId, bindings: &mut UnificationBindings,
    cache: &mut ModuleCache<'c>,
) -> OccursResult {
    match find_binding(haystack, bindings, cache) {
        Bound(binding) => occurs(needle, level, &binding, bindings, cache),
        Unbound(original_level, _) => {
            let binding = if level < original_level { vec![(needle, level)] } else { vec![] };
            OccursResult::new(needle == haystack, binding)
        },
    }
}

/// Returns what a given type is bound to, following all typevar links until it reaches an Unbound one.
pub fn follow_bindings_in_cache_and_map<'b>(
    typ: &Type, bindings: &UnificationBindings, cache: &ModuleCache<'b>,
) -> Type {
    match typ {
        TypeVariable(id) | Ref(id) => match find_binding(*id, bindings, cache) {
            Bound(typ) => follow_bindings_in_cache_and_map(&typ, bindings, cache),
            Unbound(..) => typ.clone(),
        },
        _ => typ.clone(),
    }
}

pub fn follow_bindings_in_cache<'b>(typ: &Type, cache: &ModuleCache<'b>) -> Type {
    match typ {
        TypeVariable(id) | Ref(id) => match &cache.type_bindings[id.0] {
            Bound(typ) => follow_bindings_in_cache(typ, cache),
            Unbound(..) => typ.clone(),
        },
        _ => typ.clone(),
    }
}

/// Try to unify the two given types, with the given addition set of type bindings.
/// This will not perform any binding of type variables in-place, instead it will insert
/// their mapping into the given set of bindings, letting the user of this function decide
/// whether to use the unification results or not.
///
/// If there is an error during unification, an appropriate error message is returned,
/// and the given bindings set may still be modified with prior type bindings.
///
/// This function performs the bulk of the work for the various unification functions.
#[allow(clippy::nonminimal_bool)]
pub fn try_unify_with_bindings<'b>(
    t1: &Type, t2: &Type, bindings: &mut UnificationBindings, location: Location<'b>, cache: &mut ModuleCache<'b>,
) -> Result<(), ErrorMessage<'b>> {
    match (t1, t2) {
        (Primitive(p1), Primitive(p2)) if p1 == p2 => Ok(()),

        (UserDefined(id1), UserDefined(id2)) if id1 == id2 => Ok(()),

        // Any type variable can be bound or unbound.
        // - If bound: unify the bound type with the other type.
        // - If unbound: 'unify' the LetBindingLevel of the type variable by setting
        //   it to the minimum scope of type variables in b. This happens within the occurs check.
        //   The unification of the LetBindingLevel here is a form of lifetime inference for the
        //   typevar and is used during generalization to determine which variables to generalize.
        (TypeVariable(id), _) => try_unify_type_variable_with_bindings(*id, t1, t2, bindings, location, cache),

        (_, TypeVariable(id)) => try_unify_type_variable_with_bindings(*id, t2, t1, bindings, location, cache),

        (Function(function1), Function(function2)) => {
            if function1.parameters.len() != function2.parameters.len() {
                // Whether a function is varargs or not is never unified,
                // so if one function is varargs, assume they both should be.
                if !(function1.is_varargs && function2.parameters.len() >= function1.parameters.len())
                    && !(function2.is_varargs && function1.parameters.len() >= function2.parameters.len())
                {
                    return Err(make_error!(
                        location,
                        "Function types differ in argument count: {} ({} arg(s)) and {} ({} arg(s))",
                        t1.display(cache),
                        function1.parameters.len(),
                        t2.display(cache),
                        function2.parameters.len()
                    ));
                }
            }

            for (a_arg, b_arg) in function1.parameters.iter().zip(function2.parameters.iter()) {
                try_unify_with_bindings(a_arg, b_arg, bindings, location, cache)?
            }

            try_unify_with_bindings(&function1.return_type, &function2.return_type, bindings, location, cache)?;
            try_unify_with_bindings(&function1.environment, &function2.environment, bindings, location, cache)?;
            Ok(())
        },

        (TypeApplication(a_constructor, a_args), TypeApplication(b_constructor, b_args)) => {
            if a_args.len() != b_args.len() {
                return Err(make_error!(
                    location,
                    "Arity mismatch between {} and {}",
                    t1.display(cache),
                    t2.display(cache)
                ));
            }

            try_unify_with_bindings(a_constructor, b_constructor, bindings, location, cache)?;

            for (a_arg, b_arg) in a_args.iter().zip(b_args.iter()) {
                try_unify_with_bindings(a_arg, b_arg, bindings, location, cache)?;
            }

            Ok(())
        },

        // Refs have a hidden lifetime variable we need to unify here
        (Ref(a_lifetime), Ref(_)) => {
            try_unify_type_variable_with_bindings(*a_lifetime, t1, t2, bindings, location, cache)
        },

        (a, b) => Err(make_error!(location, "Type mismatch between {} and {}", a.display(cache), b.display(cache))),
    }
}

/// Unify a single type variable (id arising from the type a) with an expected type b.
/// Follows the given TypeBindings in bindings and the cache if a is Bound.
fn try_unify_type_variable_with_bindings<'c>(
    id: TypeVariableId, a: &Type, b: &Type, bindings: &mut UnificationBindings, location: Location<'c>,
    cache: &mut ModuleCache<'c>,
) -> Result<(), ErrorMessage<'c>> {
    match find_binding(id, bindings, cache) {
        Bound(a) => try_unify_with_bindings(&a, b, bindings, location, cache),
        Unbound(a_level, _a_kind) => {
            // Create binding for boundTy that is currently empty.
            // Ensure not to create recursive bindings to the same variable
            let b = follow_bindings_in_cache_and_map(b, bindings, cache);
            if *a != b {
                let result = occurs(id, a_level, &b, bindings, cache);
                if result.occurs {
                    Err(make_error!(
                        location,
                        "Cannot construct recursive type: {} = {}",
                        a.debug(cache),
                        b.debug(cache)
                    ))
                } else {
                    bindings.bindings.insert(id, b);
                    Ok(())
                }
            } else {
                Ok(())
            }
        },
    }
}

/// A convenience wrapper for try_unify_with_bindings, creating an empty
/// set of type bindings, and returning all the newly-created bindings on success,
/// or the unification error message on error.
pub fn try_unify<'c>(
    t1: &Type, t2: &Type, location: Location<'c>, cache: &mut ModuleCache<'c>,
) -> UnificationResult<'c> {
    let mut bindings = UnificationBindings::empty();
    try_unify_with_bindings(t1, t2, &mut bindings, location, cache).map(|()| bindings)
}

/// Try to unify all the given type, with the given bindings in scope.
/// Will add new bindings to the given TypeBindings and return them all on success.
pub fn try_unify_all_with_bindings<'c>(
    vec1: &[Type], vec2: &[Type], mut bindings: UnificationBindings, location: Location<'c>,
    cache: &mut ModuleCache<'c>,
) -> UnificationResult<'c> {
    if vec1.len() != vec2.len() {
        // This bad error message is the reason this function isn't used within
        // try_unify_with_bindings! We'd need access to the full type to give better
        // errors like the other function does.
        return Err(make_error!(
            location,
            "Type-length mismatch: {} versus {} when unifying [{}] and [{}]",
            vec1.len(),
            vec2.len(),
            concat_type_strings(vec1, cache),
            concat_type_strings(vec2, cache)
        ));
    }

    for (t1, t2) in vec1.iter().zip(vec2.iter()) {
        try_unify_with_bindings(t1, t2, &mut bindings, location, cache)?;
    }
    Ok(bindings)
}

/// Concatenate all the types into a comma-separated string for error messages.
fn concat_type_strings<'c>(types: &[Type], cache: &ModuleCache<'c>) -> String {
    let types = fmap(types, |typ| typ.display(cache).to_string());
    join_with(&types, ", ")
}

/// Unifies the two given types, remembering the unification results in the cache.
/// If this operation fails, a user-facing error message is emitted.
pub fn unify<'c>(t1: &Type, t2: &Type, location: Location<'c>, cache: &mut ModuleCache<'c>) {
    perform_bindings_or_print_error(try_unify(t1, t2, location, cache), cache);
}

/// Helper for committing to the results of try_unify.
/// Places all the typevar bindings in the cache to be remembered,
/// or otherwise prints out the given error message.
pub fn perform_bindings_or_print_error<'c>(unification_result: UnificationResult<'c>, cache: &mut ModuleCache<'c>) {
    match unification_result {
        Ok(bindings) => bindings.perform(cache),
        Err(message) => eprintln!("{}", message),
    }
}

/// Remember all the given type bindings in the cache,
/// permanently binding the given type variables to the given bindings.
fn perform_type_bindings(bindings: TypeBindings, cache: &mut ModuleCache) {
    for (id, binding) in bindings.into_iter() {
        cache.type_bindings[id.0] = Bound(binding);
    }
}

fn level_is_polymorphic(level: LetBindingLevel) -> bool {
    level.0 > CURRENT_LEVEL.load(Ordering::SeqCst)
}

/// Collects all the type variables contained within typ into a Vec.
/// If polymorphic_only is true, any polymorphic type variables will be filtered out.
///
/// Since this function uses CURRENT_LEVEL when polymorphic_only = true, the function
/// should only be used with polymorphic_only = false outside of the typechecking pass.
/// Otherwise the decision of whether to propagate the variable would be incorrect.
pub fn find_all_typevars<'a>(typ: &Type, polymorphic_only: bool, cache: &ModuleCache<'a>) -> Vec<TypeVariableId> {
    match typ {
        Primitive(_) => vec![],
        UserDefined(_) => vec![],
        TypeVariable(id) => find_typevars_in_typevar_binding(*id, polymorphic_only, cache),
        Function(function) => {
            let mut type_variables = vec![];
            for parameter in &function.parameters {
                type_variables.append(&mut find_all_typevars(parameter, polymorphic_only, cache));
            }
            type_variables.append(&mut find_all_typevars(&function.environment, polymorphic_only, cache));
            type_variables.append(&mut find_all_typevars(&function.return_type, polymorphic_only, cache));
            type_variables
        },
        TypeApplication(constructor, args) => {
            let mut type_variables = find_all_typevars(constructor, polymorphic_only, cache);
            for arg in args {
                type_variables.append(&mut find_all_typevars(arg, polymorphic_only, cache));
            }
            type_variables
        },
        Ref(lifetime) => find_typevars_in_typevar_binding(*lifetime, polymorphic_only, cache),
    }
}

/// Helper for find_all_typevars which gets the TypeBinding for a given
/// TypeVariableId and either recurses on it if it is bound or returns it.
fn find_typevars_in_typevar_binding(
    id: TypeVariableId, polymorphic_only: bool, cache: &ModuleCache,
) -> Vec<TypeVariableId> {
    match &cache.type_bindings[id.0] {
        Bound(t) => find_all_typevars(t, polymorphic_only, cache),
        Unbound(level, _) => {
            if level_is_polymorphic(*level) || !polymorphic_only {
                vec![id]
            } else {
                vec![]
            }
        },
    }
}

fn find_all_typevars_in_traits<'a>(traits: &TraitConstraints, cache: &ModuleCache<'a>) -> Vec<TypeVariableId> {
    let mut typevars = vec![];
    for constraint in traits.iter() {
        for typ in constraint.args() {
            typevars.append(&mut find_all_typevars(typ, true, cache));
        }
    }
    typevars
}

/// Find all typevars declared inside the current LetBindingLevel and wrap the type in a PolyType
/// e.g.  generalize (a -> b -> b) = forall a b. a -> b -> b
fn generalize<'a>(typ: &Type, cache: &ModuleCache<'a>) -> GeneralizedType {
    let mut typevars = find_all_typevars(typ, true, cache);
    if typevars.is_empty() {
        GeneralizedType::MonoType(typ.clone())
    } else {
        // TODO: This can be sped up, e.g. we wouldn't need to dedup at all if we didn't use a Vec
        typevars.sort();
        typevars.dedup();
        GeneralizedType::PolyType(typevars, typ.clone())
    }
}

fn infer_nested_definition(
    definition_id: DefinitionInfoId, impl_scope: ImplScopeId, callsite: VariableId, cache: &mut ModuleCache,
) -> (GeneralizedType, TraitConstraints) {
    let level = LetBindingLevel(CURRENT_LEVEL.load(Ordering::SeqCst));
    let typevar = cache.next_type_variable(level);
    let info = &mut cache.definition_infos[definition_id.0];
    let definition = info.definition.as_mut().unwrap();

    // Mark the definition with a fresh typevar for recursive references
    info.typ = Some(GeneralizedType::MonoType(typevar));

    match definition {
        DefinitionKind::Definition(definition) => {
            let definition = trustme::extend_lifetime(*definition);
            infer(definition, cache);
        },
        DefinitionKind::TraitDefinition(definition) => {
            let definition = trustme::extend_lifetime(*definition);
            infer(definition, cache);
        },
        DefinitionKind::Extern(declaration) => {
            let definition = trustme::extend_lifetime(*declaration);
            infer(definition, cache);
        },
        DefinitionKind::Parameter => {},
        DefinitionKind::MatchPattern => {},
        DefinitionKind::TypeConstructor { .. } => {},
    };

    let constraints = to_trait_constraints(definition_id, impl_scope, callsite, cache);

    let info = &mut cache.definition_infos[definition_id.0];
    (info.typ.clone().unwrap(), constraints)
}

/// Infer the type of all the closed-over variables within a lambda so when we
/// type check the body their type will already be known.
fn bind_closure_environment<'c>(environment: &mut ClosureEnvironment, cache: &mut ModuleCache<'c>) {
    for (from, (_, to, to_bindings)) in environment {
        if let Some(from) = cache.definition_infos[from.0].typ.as_ref() {
            let (from, _, bindings) = from.clone().instantiate(vec![], cache);

            let to_type = &mut cache[*to].typ;
            assert!(to_type.is_none());

            // The 'to' ids are the variables used within the closure, so they should
            // be monomorphic like other function parameters are.
            *to_type = Some(GeneralizedType::MonoType(from));
            *to_bindings = Rc::new(bindings);
        }
    }
}

fn infer_closure_environment<'c>(environment: &ClosureEnvironment, cache: &mut ModuleCache<'c>) -> Type {
    let mut environment =
        fmap(environment, |(_from, (_, to, _))| cache[*to].typ.as_ref().unwrap().clone().into_monotype());

    if environment.is_empty() {
        // Non-closure functions have an environment of type unit
        Primitive(PrimitiveType::UnitType)
    } else if environment.len() == 1 {
        environment.pop().unwrap()
    } else {
        make_tuple_type(environment)
    }
}

/// Makes a tuple out of nested pairs with elements from the
/// given Vec of types. Since this is made from nested pairs
/// and includes no type terminator, it requires at least 2
/// types to be passed in.
fn make_tuple_type(mut types: Vec<Type>) -> Type {
    assert!(types.len() > 1);
    let mut ret = types.pop().unwrap();

    while !types.is_empty() {
        let typ = types.pop().unwrap();
        let pair = Box::new(Type::UserDefined(PAIR_TYPE));
        ret = Type::TypeApplication(pair, vec![typ, ret]);
    }

    ret
}

/// Binds a given type to an irrefutable pattern, recursing on the pattern and verifying
/// that it is indeed irrefutable. If should_generalize is true, this generalizes the type given
/// to any variable encountered. Appends the given required_traits list in the DefinitionInfo's
/// required_traits field.
fn bind_irrefutable_pattern<'c>(
    ast: &mut ast::Ast<'c>, typ: &Type, required_traits: &[RequiredTrait], should_generalize: bool,
    cache: &mut ModuleCache<'c>,
) {
    use ast::Ast::*;
    use ast::LiteralKind;

    match ast {
        Literal(literal) => match literal.kind {
            LiteralKind::Unit => {
                literal.set_type(Type::Primitive(PrimitiveType::UnitType));
                unify(typ, &Type::Primitive(PrimitiveType::UnitType), ast.locate(), cache);
            },
            _ => error!(ast.locate(), "Pattern is not irrefutable"),
        },
        Variable(variable) => {
            let definition_id = variable.definition.unwrap();
            let info = &cache.definition_infos[definition_id.0];

            // The type may already be set (e.g. from a trait impl this definition belongs to).
            // If it is, unify the existing type and new type before generalizing them.
            if let Some(existing_type) = &info.typ {
                match existing_type {
                    GeneralizedType::MonoType(existing_type) => {
                        unify(&existing_type.clone(), typ, variable.location, cache);
                    },
                    GeneralizedType::PolyType(_, _) => {
                        unreachable!("Cannot unify a polytype: {}", existing_type.debug(cache))
                    },
                }
            }

            let typ = if should_generalize { generalize(typ, cache) } else { GeneralizedType::MonoType(typ.clone()) };

            let info = &mut cache.definition_infos[definition_id.0];
            info.required_traits.extend_from_slice(required_traits);

            variable.typ = Some(typ.remove_forall().clone());
            info.typ = Some(typ);
        },
        TypeAnnotation(annotation) => {
            unify(typ, annotation.typ.as_ref().unwrap(), annotation.location, cache);
            bind_irrefutable_pattern(annotation.lhs.as_mut(), typ, required_traits, should_generalize, cache);
        },
        FunctionCall(call) if call.is_pair_constructor() => {
            let args = fmap(&call.args, |_| next_type_variable(cache));
            let pair_type = Box::new(Type::UserDefined(PAIR_TYPE));

            let pair_type = Type::TypeApplication(pair_type, args.clone());
            unify(typ, &pair_type, call.location, cache);

            let function_type = Type::Function(FunctionType {
                parameters: args,
                return_type: Box::new(pair_type.clone()),
                environment: Box::new(Type::Primitive(PrimitiveType::UnitType)),
                is_varargs: false,
            });

            call.function.set_type(function_type);
            call.set_type(pair_type.clone());

            match pair_type {
                Type::TypeApplication(_, args) => {
                    for (element, element_type) in call.args.iter_mut().zip(args) {
                        bind_irrefutable_pattern(element, &element_type, required_traits, should_generalize, cache);
                    }
                },
                _ => unreachable!(),
            }
        },
        _ => {
            error!(ast.locate(), "Invalid syntax in irrefutable pattern");
        },
    }
}

fn lookup_definition_type_in_trait<'a>(
    name: &str, trait_id: TraitInfoId, cache: &mut ModuleCache<'a>,
) -> GeneralizedType {
    let trait_info = &cache.trait_infos[trait_id.0];
    for definition_id in trait_info.definitions.iter() {
        let definition_info = &cache.definition_infos[definition_id.0];
        if definition_info.name == name {
            match definition_info.typ.as_ref() {
                Some(typ) => return typ.clone(),
                None => return infer_trait_definition(name, trait_id, cache),
            }
        }
    }
    unreachable!()
}

fn lookup_definition_traits_in_trait(name: &str, trait_id: TraitInfoId, cache: &mut ModuleCache) -> Vec<RequiredTrait> {
    let trait_info = &cache.trait_infos[trait_id.0];
    for definition_id in trait_info.definitions.iter() {
        let definition_info = &cache.definition_infos[definition_id.0];
        if definition_info.name == name {
            // Check if this trait definition has already been type-checked
            if definition_info.typ.is_some() {
                // TODO: Shouldn't need to clone here. Seems to be a limitation of the current
                // borrow checker.
                return definition_info.required_traits.clone();
            } else {
                return infer_trait_definition_traits(name, trait_id, cache);
            }
        }
    }
    unreachable!()
}

/// Perform type inference on the ast::TraitDefinition that defines the given trait function name.
/// The type returned will be that of the named trait member rather than the trait as a whole.
fn infer_trait_definition<'c>(name: &str, trait_id: TraitInfoId, cache: &mut ModuleCache<'c>) -> GeneralizedType {
    let trait_info = &mut cache.trait_infos[trait_id.0];
    match &mut trait_info.trait_node {
        Some(node) => {
            infer(trustme::extend_lifetime(*node), cache);
            lookup_definition_type_in_trait(name, trait_id, cache)
        },
        None => unreachable!("Type for {} has not been filled in yet", name),
    }
}

fn infer_trait_definition_traits(name: &str, trait_id: TraitInfoId, cache: &mut ModuleCache) -> Vec<RequiredTrait> {
    let trait_info = &mut cache.trait_infos[trait_id.0];
    match &mut trait_info.trait_node {
        Some(node) => {
            infer(trustme::extend_lifetime(*node), cache);
            lookup_definition_traits_in_trait(name, trait_id, cache)
        },
        None => unreachable!("Type for {} has not been filled in yet", name),
    }
}

/// Both this function and bind_irrefutable_pattern traverse an irrefutable pattern.
/// The former traverses the pattern along with a type and unifies them. This one traverses
/// the pattern and unifies any names it finds with matching names in the given TraitInfo.
/// Additionally, instead of instantiating every definition separately this function receives the
/// already-instantiated type variables from the trait impl.
///
/// Note: This function needs to be called before type inference on the trait impl definition
/// for two reasons:
///     1. Inference on Definitions performs generalization which would mean we'd otherwise need to
///        forcibly remove the forall without instantiating it to unify with trait_type here.
///     2. Binding the pattern to the definintion type from the parent trait here improves error
///        messages! Binding it beforehand leads to error messages inside the function body where
///        the e.g. return type conflicts. Binding it afterward would produce error messages with
///        the location of the ast in this function, which would just be the entire Definition.
///        Additionally, it would give the entire function type instead of just the return
///        type or parameter type that was incorrect.
fn bind_irrefutable_pattern_in_impl<'a>(
    ast: &ast::Ast<'a>, trait_id: TraitInfoId, bindings: &mut TypeBindings, cache: &mut ModuleCache<'a>,
) {
    use ast::Ast::*;
    match ast {
        Variable(variable) => {
            let name = variable.to_string();
            let trait_type = lookup_definition_type_in_trait(&name, trait_id, cache);

            let trait_type = instantiate_impl_with_bindings(&trait_type, bindings, cache);

            let definition_id = variable.definition.unwrap();
            let info = &mut cache[definition_id];
            info.typ = Some(trait_type);
        },
        TypeAnnotation(annotation) => {
            bind_irrefutable_pattern_in_impl(annotation.lhs.as_ref(), trait_id, bindings, cache);
        },
        FunctionCall(call) => {
            for arg in &call.args {
                bind_irrefutable_pattern_in_impl(arg, trait_id, bindings, cache);
            }
        },
        _ => {
            error!(ast.locate(), "Invalid syntax in irrefutable pattern in trait impl, expected a pattern of some kind (a name, type annotation, or type constructor)");
        },
    }
}

/// Checks that the traits used in `pattern` are a subset of traits used in the `given` list of
/// an impl or in the `given` list of the corresponding function in the trait declaration.
fn check_impl_propagated_traits(
    pattern: &ast::Ast, trait_id: TraitInfoId, given: &[ConstraintSignature], cache: &mut ModuleCache,
) {
    use ast::Ast::*;
    match pattern {
        Variable(variable) => {
            let name = variable.to_string();

            // Given a trait:
            // ```
            // trait Foo a with
            //     foo : a -> a
            //         given Bar a, Baz a
            // ```
            // This list will contain [Bar a, Baz a]
            let useable_traits = lookup_definition_traits_in_trait(&name, trait_id, cache);

            let definition_id = variable.definition.unwrap();
            let used_traits = cache[definition_id].required_traits.clone();
            let mut new_ids = Vec::with_capacity(used_traits.len());

            for used in used_traits {
                if let Some(id) = find_matching_trait(&used, &useable_traits, given, cache) {
                    new_ids.push(id);
                } else {
                    // TODO: Should issue this error earlier to give a better callsite for the error
                    error!(variable.location, "This definition requires {}, but the trait isn't given in the impl or the type signature for {} in the trait that defines it.",
                           used.display(cache), variable);
                }
            }

            // Must loop over again because cache is already borrowed mutably in the above loop
            for (used, new_id) in cache[definition_id].required_traits.iter_mut().zip(new_ids) {
                used.signature.id = new_id;
            }
        },
        TypeAnnotation(annotation) => check_impl_propagated_traits(&annotation.lhs, trait_id, given, cache),
        FunctionCall(call) => {
            for arg in &call.args {
                check_impl_propagated_traits(arg, trait_id, given, cache)
            }
        },
        _ => {
            error!(pattern.locate(), "Invalid syntax in irrefutable pattern in trait impl, expected a pattern of some kind (a name, type annotation, or type constructor)");
        },
    }
}

// TODO: `useable_traits` here is always going to be empty. We'll likely need a
// `Vec<ConstraintSignature>` field on each definition to account for trait definitions
// with no body.
fn find_matching_trait(
    used: &RequiredTrait, useable_traits: &[RequiredTrait], given: &[ConstraintSignature], cache: &mut ModuleCache,
) -> Option<TraitConstraintId> {
    for useable in useable_traits {
        if useable.signature.trait_id == used.signature.trait_id {
            if let Ok(bindings) = try_unify_all_with_bindings(
                &used.signature.args,
                &useable.signature.args,
                UnificationBindings::empty(),
                Location::builtin(),
                cache,
            ) {
                bindings.perform(cache);
                return Some(useable.signature.id);
            }
        }
    }

    for useable in given {
        if useable.trait_id == used.signature.trait_id {
            if let Ok(bindings) = try_unify_all_with_bindings(
                &used.signature.args,
                &useable.args,
                UnificationBindings::empty(),
                Location::builtin(),
                cache,
            ) {
                bindings.perform(cache);
                return Some(useable.id);
            }
        }
    }

    None
}

pub trait Inferable<'a> {
    fn infer_impl(&mut self, checker: &mut ModuleCache<'a>) -> (Type, TraitConstraints);
}

/// Compile an entire program, starting from main then lazily compiling
/// each used function as it is called.
pub fn infer_ast<'a>(ast: &mut ast::Ast<'a>, cache: &mut ModuleCache<'a>) {
    CURRENT_LEVEL.store(INITIAL_LEVEL, Ordering::SeqCst);
    let (_, traits) = infer(ast, cache);
    CURRENT_LEVEL.store(INITIAL_LEVEL - 1, Ordering::SeqCst);

    let exposed_traits = traitchecker::resolve_traits(traits, &[], cache);
    // No traits should be propogated above the top-level main function
    assert!(exposed_traits.is_empty());
}

pub fn infer<'a, T>(ast: &mut T, cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints)
where
    T: Inferable<'a> + Typed + std::fmt::Display,
{
    let (typ, traits) = ast.infer_impl(cache);
    ast.set_type(typ.clone());
    (typ, traits)
}

/// Note: each Ast's inference rule is given above the impl if available.
impl<'a> Inferable<'a> for ast::Ast<'a> {
    fn infer_impl(&mut self, cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints) {
        dispatch_on_expr!(self, Inferable::infer_impl, cache)
    }
}

impl<'a> Inferable<'a> for ast::Literal<'a> {
    fn infer_impl(&mut self, cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints) {
        use ast::LiteralKind::*;
        match self.kind {
            Integer(x, kind) => {
                if kind == IntegerKind::Unknown {
                    // Mutate this unknown integer literal to an IntegerKind::Inferred(int_type).
                    // Also add `Int int_type` constraint to restrict this type variable to one
                    // of the native integer types.
                    let int_type = next_type_variable_id(cache);
                    let callsite = cache.push_variable(x.to_string(), self.location);
                    let trait_impl = TraitConstraint::int_constraint(int_type, callsite, cache);
                    self.kind = Integer(x, IntegerKind::Inferred(int_type));
                    (Type::TypeVariable(int_type), vec![trait_impl])
                } else {
                    (Type::Primitive(PrimitiveType::IntegerType(kind)), vec![])
                }
            },
            Float(_) => (Type::Primitive(PrimitiveType::FloatType), vec![]),
            String(_) => (Type::UserDefined(STRING_TYPE), vec![]),
            Char(_) => (Type::Primitive(PrimitiveType::CharType), vec![]),
            Bool(_) => (Type::Primitive(PrimitiveType::BooleanType), vec![]),
            Unit => (Type::Primitive(PrimitiveType::UnitType), vec![]),
        }
    }
}

/* Var
 *   x : s ∊ cache
 *   t = instantiate s
 *   -----------
 *   infer cache x = t
 */
impl<'a> Inferable<'a> for ast::Variable<'a> {
    fn infer_impl(&mut self, cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints) {
        let definition_id = self.definition.unwrap();
        let impl_scope = self.impl_scope.unwrap();
        let id = self.id.unwrap();

        let info = &cache[definition_id];

        // Lookup the type of the definition.
        // We'll need to recursively infer the type if it is not found
        let (s, traits) = match &info.typ {
            Some(typ) => {
                let typ = typ.clone();
                let constraints = to_trait_constraints(definition_id, impl_scope, id, cache);
                (typ, constraints)
            },
            None => {
                // If the variable has a definition we can infer from then use that
                // to determine the type, otherwise fill in a type variable for it.
                let (typ, traits) = if info.definition.is_some() {
                    infer_nested_definition(self.definition.unwrap(), impl_scope, id, cache)
                } else {
                    (GeneralizedType::MonoType(next_type_variable(cache)), vec![])
                };

                let info = &mut cache.definition_infos[self.definition.unwrap().0];
                info.typ = Some(typ.clone());
                (typ, traits)
            },
        };

        let (t, traits, mapping) = s.instantiate(traits, cache);
        self.instantiation_mapping = Rc::new(mapping);
        (t, traits)
    }
}

/* Abs
 *   arg_type1 = newvar ()
 *   arg_type2 = newvar ()
 *   ...
 *   arg_typeN = newvar ()
 *   infer body (x1:arg_type1 x2:arg_type2 ... xN:arg_typeN :: cache) = return_type
 *   -------------
 *   infer (fn arg1 arg2 ... argN -> body) cache = arg_type1 arg_type2 ... arg_typeN : return_type
 */
impl<'a> Inferable<'a> for ast::Lambda<'a> {
    fn infer_impl(&mut self, cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints) {
        // The newvars for the parameters are filled out during name resolution
        let parameter_types = fmap(&self.args, |_| next_type_variable(cache));

        for (parameter, parameter_type) in self.args.iter_mut().zip(parameter_types.iter()) {
            bind_irrefutable_pattern(parameter, parameter_type, &[], false, cache);
        }

        bind_closure_environment(&mut self.closure_environment, cache);

        let (return_type, traits) = if let Some(typ) = self.body.get_type() {
            // Check if user specified a return type
            let typ = typ.clone();
            let (return_type, traits) = self.body.infer_impl(cache);
            unify(&typ, &return_type, self.location, cache);
            (typ, traits)
        } else {
            infer(self.body.as_mut(), cache)
        };

        let typ = Function(FunctionType {
            parameters: parameter_types,
            return_type: Box::new(return_type),
            environment: Box::new(infer_closure_environment(&self.closure_environment, cache)),
            is_varargs: false,
        });

        // let typevars_in_fn = find_all_typevars(&typ, false, cache);
        // let exposed_traits = traitchecker::resolve_traits(traits.clone(), &typevars_in_fn, cache);
        // self.required_traits = exposed_traits;

        // TODO: should we return exposed traits instead?
        (typ, traits)
    }
}

/* App
 *   infer cache function = f
 *   infer cache arg1 = t1
 *   infer cache arg2 = t2
 *   ...
 *   infer cache argN = tN
 *   return_type = newvar ()
 *   unify f (t1 t2 ... tN -> return_type)
 *   ---------------
 *   infer cache (function args) = return_type
 */
impl<'a> Inferable<'a> for ast::FunctionCall<'a> {
    fn infer_impl(&mut self, cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints) {
        let (f, mut traits) = infer(self.function.as_mut(), cache);
        let (parameters, mut arg_traits) = fmap_mut_pair_flatten_second(&mut self.args, |arg| infer(arg, cache));

        let return_type = next_type_variable(cache);
        traits.append(&mut arg_traits);

        let new_function = Function(FunctionType {
            parameters,
            return_type: Box::new(return_type.clone()),
            environment: Box::new(next_type_variable(cache)),
            is_varargs: false,
        });

        unify(&f, &new_function, self.location, cache);
        (return_type, traits)
    }
}

/// True if the expression can be generalized. Generalizing expressions
/// will cause them to be re-evaluated whenever they're used with new types,
/// so generalization should be limited to when this would be expected by
/// users (functions) or when it would not be noticeable (variables).
fn should_generalize(ast: &ast::Ast) -> bool {
    match ast {
        ast::Ast::Variable(_) => true,
        ast::Ast::Lambda(lambda) => lambda.closure_environment.is_empty(),
        _ => false,
    }
}

/* Let
 *   infer cache expr = t
 *   infer (pattern:(generalize t) :: cache) rest = t'
 *   -----------------
 *   infer cache (let pattern = expr in rest) = t'
 */
impl<'a> Inferable<'a> for ast::Definition<'a> {
    fn infer_impl(&mut self, cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints) {
        let unit = Type::Primitive(PrimitiveType::UnitType);

        if self.typ.is_some() {
            return (unit, vec![]);
        } else {
            // Without this self.typ wouldn't be set yet while inferring the type of self.expr
            // if this definition is recursive. If this is removed we would recursively infer
            // this definition repeatedly until eventually reaching an error when the previous type
            // is generalized but the new one is not.
            self.typ = Some(unit.clone());
        }

        let level = self.level.unwrap();
        let previous_level = CURRENT_LEVEL.swap(level.0, Ordering::SeqCst);

        // The rhs of a Definition must be inferred at a greater LetBindingLevel than
        // the lhs below. Here we use level for the rhs and level - 1 for the lhs
        let (t, traits) = infer(self.expr.as_mut(), cache);

        CURRENT_LEVEL.store(level.0 - 1, Ordering::SeqCst);

        // TODO: the inferred type t needs to be unified with the patterns type before
        // resolve_traits is called. For now it is sufficient to call bind_irrefutable_pattern
        // twice - the first time with no traits, however in the future bind_irrefutable_pattern
        // should be split up into two parts.
        bind_irrefutable_pattern(self.pattern.as_mut(), &t, &[], false, cache);

        // TODO investigate this check, should be unneeded. It is breaking on the `input` function
        // in the stdlib.
        if self.pattern.get_type().is_none() {
            self.pattern.set_type(t.clone());
        }

        // If this definition is of a lambda or variable we try to generalize it,
        // which entails wrapping type variables in a forall, and finding which traits
        // usages of this definitio require.
        let traits = if should_generalize(self.expr.as_ref()) {
            let typevars_in_fn = find_all_typevars(self.pattern.get_type().unwrap(), false, cache);
            let exposed_traits = traitchecker::resolve_traits(traits, &typevars_in_fn, cache);

            bind_irrefutable_pattern(self.pattern.as_mut(), &t, &exposed_traits, true, cache);
            vec![]
        } else {
            traits
        };

        // TODO: Can these operations on the LetBindingLevel be simplified?
        CURRENT_LEVEL.store(previous_level, Ordering::SeqCst);
        (unit, traits)
    }
}

impl<'a> Inferable<'a> for ast::If<'a> {
    fn infer_impl(&mut self, cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints) {
        let (condition, mut traits) = infer(self.condition.as_mut(), cache);
        let bool_type = Type::Primitive(PrimitiveType::BooleanType);
        unify(&condition, &bool_type, self.condition.locate(), cache);

        let (then, mut then_traits) = infer(self.then.as_mut(), cache);
        traits.append(&mut then_traits);

        if let Some(otherwise) = &mut self.otherwise {
            let (otherwise, mut otherwise_traits) = infer(otherwise.as_mut(), cache);
            traits.append(&mut otherwise_traits);

            unify(&then, &otherwise, self.location, cache);
            (then, traits)
        } else {
            (Type::Primitive(PrimitiveType::UnitType), traits)
        }
    }
}

impl<'a> Inferable<'a> for ast::Match<'a> {
    fn infer_impl(&mut self, cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints) {
        let error_count = get_error_count();

        let (expression, mut traits) = infer(self.expression.as_mut(), cache);
        let mut return_type = Type::Primitive(PrimitiveType::UnitType);

        if !self.branches.is_empty() {
            // Unroll the first iteration of inferring (pattern, branch) types so each
            // subsequent (pattern, branch) types can be unified against the first.
            let (pattern_type, mut pattern_traits) = infer(&mut self.branches[0].0, cache);

            traits.append(&mut pattern_traits);
            unify(&expression, &pattern_type, self.branches[0].0.locate(), cache);

            let (branch, mut branch_traits) = infer(&mut self.branches[0].1, cache);
            return_type = branch;
            traits.append(&mut branch_traits);

            for (pattern, branch) in self.branches.iter_mut().skip(1) {
                let (pattern_type, mut pattern_traits) = infer(pattern, cache);
                let (branch_type, mut branch_traits) = infer(branch, cache);
                unify(&expression, &pattern_type, pattern.locate(), cache);
                unify(&return_type, &branch_type, branch.locate(), cache);
                traits.append(&mut pattern_traits);
                traits.append(&mut branch_traits);
            }
        }

        // Compiling the decision tree for this pattern requires each pattern is well-typed.
        // So skip this step if there was an error in inferring types for this match expression.
        if get_error_count() == error_count {
            let mut tree = pattern::compile(self, cache);
            // TODO: Infer new variables created by a decision tree within pattern::compile.
            //       It is done separately currently only for convenience/ease of implementation.
            tree.infer(self.expression.get_type().unwrap(), self.location, cache);
            self.decision_tree = Some(tree);
        }

        (return_type, traits)
    }
}

impl<'a> Inferable<'a> for ast::TypeDefinition<'a> {
    fn infer_impl(&mut self, _cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints) {
        (Type::Primitive(PrimitiveType::UnitType), vec![])
    }
}

impl<'a> Inferable<'a> for ast::TypeAnnotation<'a> {
    fn infer_impl(&mut self, cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints) {
        let (typ, traits) = infer(self.lhs.as_mut(), cache);
        unify(&typ, self.typ.as_mut().unwrap(), self.location, cache);
        (typ, traits)
    }
}

impl<'a> Inferable<'a> for ast::Import<'a> {
    /// Type checker doesn't need to follow imports.
    /// It typechecks definitions as-needed when it finds a variable whose type is still unknown.
    fn infer_impl(&mut self, _cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints) {
        (Type::Primitive(PrimitiveType::UnitType), vec![])
    }
}

impl<'a> Inferable<'a> for ast::TraitDefinition<'a> {
    fn infer_impl(&mut self, cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints) {
        let previous_level = CURRENT_LEVEL.swap(self.level.unwrap().0, Ordering::SeqCst);

        for declaration in self.declarations.iter_mut() {
            let rhs = declaration.typ.as_ref().unwrap();

            bind_irrefutable_pattern(declaration.lhs.as_mut(), rhs, &[], true, cache);
        }

        CURRENT_LEVEL.store(previous_level, Ordering::SeqCst);
        (Type::Primitive(PrimitiveType::UnitType), vec![])
    }
}

impl<'a> Inferable<'a> for ast::TraitImpl<'a> {
    fn infer_impl(&mut self, cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints) {
        if self.typ.is_some() {
            return (Type::Primitive(PrimitiveType::UnitType), vec![]);
        }

        let trait_info = &cache.trait_infos[self.trait_info.unwrap().0];

        let mut typevars_to_replace = trait_info.typeargs.clone();
        typevars_to_replace.append(&mut trait_info.fundeps.clone());

        // Need to replace all typevars here so we do not rebind over them.
        // E.g. an impl for `Cmp a given Int a` could be accidentally bound to `Cmp usz`
        let (trait_arg_types, _) = replace_all_typevars(&self.trait_arg_types, cache);

        // Instantiate the typevars in the parent trait to bind their definition
        // types against the types in this trait impl. This needs to be done once
        // at the trait level rather than at each definition so that each definition
        // refers to the same type variable instances/bindings.
        //
        // This is because only these bindings in trait_to_impl are unified against
        // the types declared in self.typeargs
        let mut impl_bindings: HashMap<_, _> = typevars_to_replace.into_iter().zip(trait_arg_types).collect();

        for definition in self.definitions.iter_mut() {
            bind_irrefutable_pattern_in_impl(
                definition.pattern.as_ref(),
                self.trait_info.unwrap(),
                &mut impl_bindings,
                cache,
            );

            let (_, traits) = infer(definition, cache);

            // Need to check we only use traits that are `given` by the definition
            // in question or by the overall impl.
            check_impl_propagated_traits(
                definition.pattern.as_ref(),
                self.trait_info.unwrap(),
                &cache[self.impl_id.unwrap()].given.clone(),
                cache,
            );

            // No traits should be propagated outside of the impl. The only way this can happen
            // is if the definition is not generalized and traits are used.
            for trait_ in traits {
                error!(
                    definition.location,
                    "Definition requires {}, but it needs to be a function to add this trait",
                    trait_.display(cache)
                );
            }
        }

        (Type::Primitive(PrimitiveType::UnitType), vec![])
    }
}

impl<'a> Inferable<'a> for ast::Return<'a> {
    fn infer_impl(&mut self, cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints) {
        let traits = infer(self.expression.as_mut(), cache).1;
        (next_type_variable(cache), traits)
    }
}

impl<'a> Inferable<'a> for ast::Sequence<'a> {
    fn infer_impl(&mut self, cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints) {
        let ignore_len = self.statements.len() - 1;
        let mut traits = vec![];

        for statement in self.statements.iter_mut().take(ignore_len) {
            let (_, mut statement_traits) = infer(statement, cache);
            traits.append(&mut statement_traits);
        }

        let (last_statement_type, mut statement_traits) = infer(self.statements.last_mut().unwrap(), cache);
        traits.append(&mut statement_traits);
        (last_statement_type, traits)
    }
}

impl<'a> Inferable<'a> for ast::Extern<'a> {
    fn infer_impl(&mut self, cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints) {
        let previous_level = CURRENT_LEVEL.swap(self.level.unwrap().0, Ordering::SeqCst);
        for declaration in self.declarations.iter_mut() {
            bind_irrefutable_pattern(declaration.lhs.as_mut(), declaration.typ.as_ref().unwrap(), &[], true, cache);
        }
        CURRENT_LEVEL.store(previous_level, Ordering::SeqCst);
        (Type::Primitive(PrimitiveType::UnitType), vec![])
    }
}

impl<'a> Inferable<'a> for ast::MemberAccess<'a> {
    /// Member access (e.g. foo.bar) in ante implies a corresponding trait constraint
    /// that is automatically implemented by the compiler. This is to allow multiple
    /// conflicting field names in a scope. For example a function:
    ///
    /// foo bar =
    ///    bar.x + 2
    ///
    /// Has the type
    ///
    /// bar : a -> int
    ///   given .x a int
    ///
    /// This given trait constraint is a member access constraint denoting that
    /// type a must have a field x of type int.
    fn infer_impl(&mut self, cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints) {
        let (collection_type, mut traits) = infer(self.lhs.as_mut(), cache);

        let level = LetBindingLevel(CURRENT_LEVEL.load(Ordering::SeqCst));
        let trait_id = cache.get_member_access_trait(&self.field, level);

        let field_type = cache.next_type_variable(level);

        let typeargs = vec![collection_type, field_type.clone()];
        let callsite = cache.push_variable(format!(".{}", self.field), self.location);
        let trait_impl = TraitConstraint::member_access_constraint(trait_id, typeargs, callsite, cache);
        traits.push(trait_impl);

        (field_type, traits)
    }
}

impl<'a> Inferable<'a> for ast::Assignment<'a> {
    fn infer_impl(&mut self, cache: &mut ModuleCache<'a>) -> (Type, TraitConstraints) {
        let mut traits = infer(self.lhs.as_mut(), cache).1;
        traits.append(&mut infer(self.rhs.as_mut(), cache).1);
        (Type::Primitive(PrimitiveType::UnitType), traits)
    }
}
