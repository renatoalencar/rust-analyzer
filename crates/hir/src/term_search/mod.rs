//! Term search

use hir_def::type_ref::Mutability;
use hir_ty::db::HirDatabase;
use itertools::Itertools;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{ModuleDef, ScopeDef, Semantics, SemanticsScope, Type};

pub mod type_tree;
pub use type_tree::TypeTree;

mod tactics;

/// # Maximum amount of variations to take per type
///
/// This is to speed up term search as there may be huge amount of variations of arguments for
/// function, even when the return type is always the same. The idea is to take first n and call it
/// a day.
const MAX_VARIATIONS: usize = 10;

/// Key for lookup table to query new types reached.
#[derive(Debug, Hash, PartialEq, Eq)]
enum NewTypesKey {
    ImplMethod,
    StructProjection,
}

/// # Lookup table for term search
///
/// Lookup table keeps all the state during term search.
/// This means it knows what types and how are reachable.
///
/// The secondary functionality for lookup table is to keep track of new types reached since last
/// iteration as well as keeping track of which `ScopeDef` items have been used.
/// Both of them are to speed up the term search by leaving out types / ScopeDefs that likely do
/// not produce any new results.
#[derive(Default, Debug)]
struct LookupTable {
    /// All the `TypeTree`s in "value" produce the type of "key"
    data: FxHashMap<Type, FxHashSet<TypeTree>>,
    /// New types reached since last query by the `NewTypesKey`
    new_types: FxHashMap<NewTypesKey, Vec<Type>>,
    /// ScopeDefs that are not interesting any more
    exhausted_scopedefs: FxHashSet<ScopeDef>,
    /// ScopeDefs that were used in current round
    round_scopedef_hits: FxHashSet<ScopeDef>,
    /// Amount of rounds since scopedef was first used.
    rounds_since_sopedef_hit: FxHashMap<ScopeDef, u32>,
}

impl LookupTable {
    /// Initialize lookup table
    fn new() -> Self {
        let mut res: Self = Default::default();
        res.new_types.insert(NewTypesKey::ImplMethod, Vec::new());
        res.new_types.insert(NewTypesKey::StructProjection, Vec::new());
        res
    }

    /// Find all `TypeTree`s that unify with the `ty`
    fn find(&self, db: &dyn HirDatabase, ty: &Type) -> Option<Vec<TypeTree>> {
        self.data
            .iter()
            .find(|(t, _)| t.could_unify_with_deeply(db, ty))
            .map(|(_, tts)| tts.iter().cloned().collect())
    }

    /// Same as find but automatically creates shared reference of types in the lookup
    ///
    /// For example if we have type `i32` in data and we query for `&i32` it map all the type
    /// trees we have for `i32` with `TypeTree::Reference` and returns them.
    fn find_autoref(&self, db: &dyn HirDatabase, ty: &Type) -> Option<Vec<TypeTree>> {
        self.data
            .iter()
            .find(|(t, _)| t.could_unify_with_deeply(db, ty))
            .map(|(_, tts)| tts.iter().cloned().collect())
            .or_else(|| {
                self.data
                    .iter()
                    .find(|(t, _)| {
                        Type::reference(t, Mutability::Shared).could_unify_with_deeply(db, &ty)
                    })
                    .map(|(_, tts)| {
                        tts.iter().map(|tt| TypeTree::Reference(Box::new(tt.clone()))).collect()
                    })
            })
    }

    /// Insert new type trees for type
    ///
    /// Note that the types have to be the same, unification is not enough as unification is not
    /// transitive. For example Vec<i32> and FxHashSet<i32> both unify with Iterator<Item = i32>,
    /// but they clearly do not unify themselves.
    fn insert(&mut self, ty: Type, trees: impl Iterator<Item = TypeTree>) {
        match self.data.get_mut(&ty) {
            Some(it) => it.extend(trees.take(MAX_VARIATIONS)),
            None => {
                self.data.insert(ty.clone(), trees.take(MAX_VARIATIONS).collect());
                for it in self.new_types.values_mut() {
                    it.push(ty.clone());
                }
            }
        }
    }

    /// Iterate all the reachable types
    fn iter_types(&self) -> impl Iterator<Item = Type> + '_ {
        self.data.keys().cloned()
    }

    /// Query new types reached since last query by key
    ///
    /// Create new key if you wish to query it to avoid conflicting with existing queries.
    fn new_types(&mut self, key: NewTypesKey) -> Vec<Type> {
        match self.new_types.get_mut(&key) {
            Some(it) => std::mem::take(it),
            None => Vec::new(),
        }
    }

    /// Mark `ScopeDef` as exhausted meaning it is not interesting for us any more
    fn mark_exhausted(&mut self, def: ScopeDef) {
        self.exhausted_scopedefs.insert(def);
    }

    /// Mark `ScopeDef` as used meaning we managed to produce something useful from it
    fn mark_fulfilled(&mut self, def: ScopeDef) {
        self.round_scopedef_hits.insert(def);
    }

    /// Start new round (meant to be called at the beginning of iteration in `term_search`)
    ///
    /// This functions marks some `ScopeDef`s as exhausted if there have been
    /// `MAX_ROUNDS_AFTER_HIT` rounds after first using a `ScopeDef`.
    fn new_round(&mut self) {
        for def in &self.round_scopedef_hits {
            let hits =
                self.rounds_since_sopedef_hit.entry(*def).and_modify(|n| *n += 1).or_insert(0);
            const MAX_ROUNDS_AFTER_HIT: u32 = 2;
            if *hits > MAX_ROUNDS_AFTER_HIT {
                self.exhausted_scopedefs.insert(*def);
            }
        }
        self.round_scopedef_hits.clear();
    }

    /// Get exhausted `ScopeDef`s
    fn exhausted_scopedefs(&self) -> &FxHashSet<ScopeDef> {
        &self.exhausted_scopedefs
    }
}

/// # Term search
///
/// Search for terms (expressions) that unify with the `goal` type.
///
/// # Arguments
/// * `sema` - Semantics for the program
/// * `scope` - Semantic scope, captures context for the term search
/// * `goal` - Target / expected output type
///
/// Internally this function uses Breadth First Search to find path to `goal` type.
/// The general idea is following:
/// 1. Populate lookup (frontier for BFS) from values (local variables, statics, constants, etc)
///    as well as from well knows values (such as `true/false` and `()`)
/// 2. Iteratively expand the frontier (or contents of the lookup) by trying different type
///    transformation tactics. For example functions take as from set of types (arguments) to some
///    type (return type). Other transformations include methods on type, type constructors and
///    projections to struct fields (field access).
/// 3. Once we manage to find path to type we are interested in we continue for single round to see
///    if we can find more paths that take us to the `goal` type.
/// 4. Return all the paths (type trees) that take us to the `goal` type.
///
/// Note that there are usually more ways we can get to the `goal` type but some are discarded to
/// reduce the memory consumption. It is also unlikely anyone is willing ti browse through
/// thousands of possible responses so we currently take first 10 from every tactic.
pub fn term_search<DB: HirDatabase>(
    sema: &Semantics<'_, DB>,
    scope: &SemanticsScope<'_>,
    goal: &Type,
) -> Vec<TypeTree> {
    let mut defs = FxHashSet::default();
    defs.insert(ScopeDef::ModuleDef(ModuleDef::Module(scope.module())));

    scope.process_all_names(&mut |_, def| {
        defs.insert(def);
    });
    let module = scope.module();

    let mut lookup = LookupTable::new();

    // Try trivial tactic first, also populates lookup table
    let mut solutions: Vec<TypeTree> =
        tactics::trivial(sema.db, &defs, &mut lookup, goal).collect();
    // Use well known types tactic before iterations as it does not depend on other tactics
    solutions.extend(tactics::famous_types(sema.db, &module, &defs, &mut lookup, goal));

    let mut solution_found = !solutions.is_empty();

    for _ in 0..5 {
        lookup.new_round();

        solutions.extend(tactics::type_constructor(sema.db, &module, &defs, &mut lookup, goal));
        solutions.extend(tactics::free_function(sema.db, &module, &defs, &mut lookup, goal));
        solutions.extend(tactics::impl_method(sema.db, &module, &defs, &mut lookup, goal));
        solutions.extend(tactics::struct_projection(sema.db, &module, &defs, &mut lookup, goal));

        // Break after 1 round after successful solution
        if solution_found {
            break;
        }

        solution_found = !solutions.is_empty();

        // Discard not interesting `ScopeDef`s for speedup
        for def in lookup.exhausted_scopedefs() {
            defs.remove(def);
        }
    }

    solutions.into_iter().unique().collect()
}
