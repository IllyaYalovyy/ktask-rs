//! Which generations a cleanup retires. A pure function of what the scan found, so every
//! rule (warm window, grace, protection, capacity) is decided without the filesystem.
use super::Policy;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::{Duration, SystemTime},
};

/// Allocated bytes and directory entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Totals {
    pub bytes: u64,
    pub files: u64,
}

impl Totals {
    pub fn add(&mut self, other: Self) {
        self.bytes += other.bytes;
        self.files += other.files;
    }
    pub fn sub(&mut self, other: Self) {
        self.bytes = self.bytes.saturating_sub(other.bytes);
        self.files = self.files.saturating_sub(other.files);
    }
    pub fn within(self, policy: &Policy) -> bool {
        self.bytes <= policy.max_bytes && self.files <= policy.max_files
    }
}

/// What Cargo produced for a unit. Cargo names a unit by target alone (`lib-x`), so a
/// `cargo check` and a `cargo build` of one library share the name and differ here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Output {
    Executable,
    Library,
    Metadata,
}

/// One Cargo compilation unit as its `.fingerprint/<package>-<hash>/` directory records it.
/// Versions of one unit replace each other; distinct units never do.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Unit {
    pub package: String,
    /// Cargo's unit name: `lib-<t>`, `test-lib-<t>`, `bin-<t>`, `test-bin-<t>`,
    /// `test-integration-test-<t>`…
    pub name: String,
    pub output: Output,
    /// Produced under clippy-driver (`# env-dep:CLIPPY_*` in its dependency-info).
    pub clippy: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Unit(Unit),
    /// rustc's incremental state. It names only the crate, so its warm window is as wide as
    /// the number of that crate's units the plan keeps.
    Incremental,
    /// Output Cargo can no longer reuse: its fingerprint or every output is gone.
    Remnant,
}

/// One removable version of something Cargo builds, with what removing it frees.
#[derive(Clone, Debug)]
pub(super) struct Generation {
    pub kind: Kind,
    /// The profile directory (`debug`, `<triple>/release`) it belongs to.
    pub profile: PathBuf,
    /// rustc's crate name, which ties incremental state to units.
    pub crate_name: String,
    /// Built from the project's own sources; registry crates are evicted only on pressure.
    pub local: bool,
    /// Open, mapped, running, a final output or an active incremental session.
    pub in_use: bool,
    pub newest: SystemTime,
    pub size: Totals,
    /// Removed in this order, each with its measured size.
    pub paths: Vec<(PathBuf, Totals)>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Plan {
    /// Generations to remove, oldest first.
    pub retire: Vec<usize>,
    /// Retired because a newer version replaced them or Cargo cannot reuse them.
    pub obsolete: u64,
    pub protected: u64,
    pub after: Totals,
}

type Group<'a> = (&'a PathBuf, &'a str, Option<&'a Unit>);

fn group(generation: &Generation) -> Option<Group<'_>> {
    let unit = match &generation.kind {
        Kind::Unit(unit) => Some(unit),
        Kind::Incremental => None,
        Kind::Remnant => return None,
    };
    Some((&generation.profile, &generation.crate_name, unit))
}

/// Keeps the newest `keep_generations` versions of every local unit and as many incremental
/// sessions per crate as units of it stay, never touches protected or fresh output, and then
/// evicts the oldest others, never the newest of a group, while over a limit.
pub(super) fn plan(
    generations: &[Generation],
    policy: &Policy,
    now: SystemTime,
    before: Totals,
) -> Plan {
    let grace = Duration::from_secs(policy.grace_secs);
    let protected: Vec<bool> = generations
        .iter()
        .map(|g| g.in_use || now.duration_since(g.newest).unwrap_or_default() < grace)
        .collect();
    let mut groups: HashMap<Group<'_>, Vec<usize>> = HashMap::new();
    let mut retire = HashSet::new();
    for (index, generation) in generations.iter().enumerate() {
        match group(generation) {
            Some(key) => groups.entry(key).or_default().push(index),
            None if !protected[index] => {
                retire.insert(index);
            }
            None => {}
        }
    }
    for members in groups.values_mut() {
        members.sort_by(|a, b| generations[*b].newest.cmp(&generations[*a].newest));
    }
    let keep = usize::try_from(policy.keep_generations).unwrap_or(usize::MAX);
    let warm = |members: &[usize], window: usize, retire: &mut HashSet<usize>| {
        let local = members.iter().any(|i| generations[*i].local);
        let mut kept = 0;
        for (rank, &index) in members.iter().enumerate() {
            if rank >= window && local && !protected[index] {
                retire.insert(index);
            } else {
                kept += 1;
            }
        }
        if local { kept } else { 0 }
    };
    let mut units_kept: HashMap<(&PathBuf, &str), usize> = HashMap::new();
    for (&(profile, crate_name, unit), members) in &groups {
        if unit.is_some() {
            *units_kept.entry((profile, crate_name)).or_default() +=
                warm(members, keep, &mut retire);
        }
    }
    for (&(profile, crate_name, unit), members) in &groups {
        if unit.is_none() {
            let window = units_kept
                .get(&(profile, crate_name))
                .map_or(keep, |kept| (*kept).max(keep));
            warm(members, window, &mut retire);
        }
    }
    let obsolete = retire.len() as u64;
    let mut after = before;
    for &index in &retire {
        after.sub(generations[index].size);
    }
    if !after.within(policy) {
        let newest: HashSet<usize> = groups.values().map(|members| members[0]).collect();
        let mut candidates: Vec<usize> = (0..generations.len())
            .filter(|i| {
                !retire.contains(i)
                    && !protected[*i]
                    && !newest.contains(i)
                    && generations[*i].kind != Kind::Remnant
            })
            .collect();
        candidates.sort_by_key(|i| generations[*i].newest);
        for index in candidates {
            if after.within(policy) {
                break;
            }
            after.sub(generations[index].size);
            retire.insert(index);
        }
    }
    let mut retire: Vec<usize> = retire.into_iter().collect();
    retire.sort_by_key(|i| (generations[*i].newest, *i));
    Plan {
        retire,
        obsolete,
        protected: protected.iter().filter(|p| **p).count() as u64,
        after,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000;
    const MIB: u64 = super::super::MIB;

    fn at(age_secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(NOW - age_secs)
    }

    fn unit(name: &str, output: Output, clippy: bool) -> Kind {
        Kind::Unit(Unit {
            package: "demo".into(),
            name: name.into(),
            output,
            clippy,
        })
    }

    fn generation(kind: Kind, crate_name: &str, age_secs: u64) -> Generation {
        Generation {
            kind,
            profile: PathBuf::from("debug"),
            crate_name: crate_name.into(),
            local: true,
            in_use: false,
            newest: at(age_secs),
            size: Totals {
                bytes: MIB,
                files: 1,
            },
            paths: Vec::new(),
        }
    }

    fn policy(keep: u64) -> Policy {
        Policy {
            keep_generations: keep,
            grace_secs: 0,
            ..Policy::default()
        }
    }

    fn run(generations: &[Generation], policy: &Policy) -> Plan {
        let mut before = Totals::default();
        for g in generations {
            before.add(g.size);
        }
        plan(generations, policy, at(0), before)
    }

    /// Every unit kind cargo check, clippy, build and test --no-run leave for one crate.
    fn current_units(age_secs: u64) -> Vec<Generation> {
        [
            unit("lib-demo", Output::Metadata, false),
            unit("lib-demo", Output::Metadata, true),
            unit("lib-demo", Output::Library, false),
            unit("test-lib-demo", Output::Metadata, false),
            unit("test-lib-demo", Output::Metadata, true),
            unit("test-lib-demo", Output::Executable, false),
            unit("bin-demo", Output::Executable, false),
            unit("test-bin-demo", Output::Executable, false),
            unit("test-integration-test-tui", Output::Executable, false),
        ]
        .into_iter()
        .map(|kind| generation(kind, "demo", age_secs))
        .collect()
    }

    #[test]
    fn distinct_units_of_one_crate_are_never_versions_of_each_other() {
        let units = current_units(100);
        let plan = run(&units, &policy(1));
        assert_eq!(plan.retire, Vec::<usize>::new());
        assert_eq!(plan.obsolete, 0);
    }

    #[test]
    fn keep_retains_the_newest_versions_of_each_unit() {
        let keep = 2;
        let versions = 4;
        let mut generations = Vec::new();
        for version in 0..versions {
            generations.extend(current_units(1_000 * (versions - version)));
        }
        let per_version = current_units(0).len();
        let plan = run(&generations, &policy(keep));
        let retired_versions = versions - keep;
        assert_eq!(
            plan.retire.len() as u64,
            retired_versions * per_version as u64
        );
        // The two oldest versions go, oldest first.
        let expected: Vec<usize> = (0..retired_versions as usize * per_version).collect();
        let mut retired = plan.retire.clone();
        retired.sort_unstable();
        assert_eq!(retired, expected);
        assert!(
            generations[plan.retire[0]].newest <= generations[*plan.retire.last().unwrap()].newest
        );
    }

    #[test]
    fn incremental_state_is_kept_for_every_kept_unit_of_its_crate() {
        let versions = 3;
        let mut generations = Vec::new();
        for version in 0..versions {
            let age = 1_000 * (versions - version);
            generations.extend(current_units(age));
            for _ in 0..current_units(0).len() {
                generations.push(generation(Kind::Incremental, "demo", age));
            }
        }
        let units_per_version = current_units(0).len();
        let plan = run(&generations, &policy(1));
        let kept_incremental = generations
            .iter()
            .enumerate()
            .filter(|(i, g)| g.kind == Kind::Incremental && !plan.retire.contains(i))
            .count();
        assert_eq!(kept_incremental, units_per_version);
        // An incremental session for a crate no unit names keeps the warm window.
        let orphans: Vec<_> = (0..3)
            .map(|v| generation(Kind::Incremental, "renamed", 1_000 * (3 - v)))
            .collect();
        assert_eq!(run(&orphans, &policy(1)).retire, vec![0, 1]);
    }

    #[test]
    fn grace_keeps_fresh_output_of_any_age_rank() {
        let grace = 600;
        let generations: Vec<_> = [5_000, grace + 1, grace - 1, 10]
            .iter()
            .map(|age| generation(unit("lib-demo", Output::Library, false), "demo", *age))
            .collect();
        let plan = run(
            &generations,
            &Policy {
                grace_secs: grace,
                ..policy(1)
            },
        );
        // Newest (age 10) is the warm window; age grace-1 is fresh; the rest retire.
        assert_eq!(plan.retire, vec![0, 1]);
        assert_eq!(plan.protected, 2);
    }

    #[test]
    fn protected_generations_and_registry_crates_are_not_retired_by_the_window() {
        let mut generations: Vec<_> = (0..4)
            .map(|v| {
                generation(
                    unit("lib-demo", Output::Library, false),
                    "demo",
                    1_000 * (4 - v),
                )
            })
            .collect();
        generations[1].in_use = true;
        let registry: Vec<_> = (0..4)
            .map(|v| {
                let mut g = generation(
                    unit("lib-serde", Output::Library, false),
                    "serde",
                    1_000 * (4 - v),
                );
                g.local = false;
                g
            })
            .collect();
        let start = generations.len();
        generations.extend(registry);
        let plan = run(&generations, &policy(1));
        assert_eq!(plan.retire, vec![0, 2]);
        assert_eq!(plan.protected, 1);
        assert!(plan.retire.iter().all(|i| *i < start));
    }

    #[test]
    fn remnants_retire_unless_protected() {
        let mut remnant = generation(Kind::Remnant, "demo", 5_000);
        let plan = run(std::slice::from_ref(&remnant), &policy(3));
        assert_eq!((plan.retire, plan.obsolete), (vec![0], 1));
        remnant.in_use = true;
        assert!(run(&[remnant], &policy(3)).retire.is_empty());
    }

    #[test]
    fn capacity_evicts_oldest_first_but_never_the_newest_of_a_unit() {
        let versions = 5;
        let mut generations: Vec<_> = (0..versions)
            .map(|v| {
                let mut g = generation(
                    unit("lib-big", Output::Library, false),
                    "big",
                    1_000 * (versions - v),
                );
                g.local = false;
                g.size.bytes = 20 * MIB;
                g
            })
            .collect();
        let limit = 64 * MIB;
        let limited = Policy {
            max_bytes: limit,
            ..policy(32)
        };
        let plan = run(&generations, &limited);
        // 100 MiB over a 64 MiB limit: the two oldest 20 MiB generations go.
        assert_eq!(plan.retire, vec![0, 1]);
        assert_eq!(plan.obsolete, 0);
        assert!(plan.after.within(&limited) && plan.after.bytes <= limit);
        // A single newest generation above the limit stays and the pressure is reported.
        generations.truncate(1);
        generations[0].size.bytes = 80 * MIB;
        let plan = run(&generations, &limited);
        assert!(plan.retire.is_empty() && !plan.after.within(&limited));
        // Protected generations are never evicted either.
        let mut pair: Vec<_> = (0..2)
            .map(|v| {
                let mut g = generation(
                    unit("lib-big", Output::Library, false),
                    "big",
                    1_000 * (2 - v),
                );
                g.size.bytes = 40 * MIB;
                g.in_use = true;
                g
            })
            .collect();
        assert!(!run(&pair, &limited).after.within(&limited));
        pair[0].in_use = false;
        assert_eq!(run(&pair, &limited).retire, vec![0]);
    }
}
