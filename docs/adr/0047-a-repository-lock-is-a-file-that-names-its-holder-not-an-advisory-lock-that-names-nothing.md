# 0047. A repository lock is a file that names its holder, not an advisory lock that names nothing

- **Status:** accepted
- **Date:** 2026-09-18

## Context

VISION.md §10 step 5 publishes "under a repository lock that serializes all
integration and publication operations", and §4 makes acquiring that lock one of
the things `preflight` proves before a run spends tokens. The serialization that
matters is across **processes**: a second TUI opened against the same project, or
a run started by hand while the first is wedged, would otherwise fetch, merge and
push mainline from two directions. T051 asks for
`acquire(dir, timeout) -> Result<RepoLock>`, and its done-when names the two
properties that pull against each other — a second acquire must block and then
time out with a clear error, *and* a lock whose pid is not alive must be
reclaimed **and the reclamation reported**.

**Every lock's real failure is the crash while holding it.** Exclusivity is easy;
the interesting state is "the file says somebody holds this and the process that
wrote it is gone". A lock with no rule for that state turns one crashed run into a
permanent lockout of the machine, which is the opposite of what §10 is for. So the
mechanism is chosen by how well it answers three questions after a crash: who held
it, can they still be holding it, and what may be said about taking it away.

**Measured (util-linux `flock` 2.40, Linux): an advisory lock outlives the process
that took it.** `flock -x lk -c 'sleep 400 &'` — the `flock` process exits, its
child is reparented to pid 1 still holding the inherited descriptor, and a second
non-blocking `flock` on the same file then fails with status 1. A lock is held by
an *open file description*, and `fork` hands that description to every child.
Under ADR-0038 the supervisor starts every gate and every agent as its own process
group from its own fds, so an advisory lock taken before that would be held by
everything the run ever spawned and would be released only when the last of them
exited. The kernel's own staleness rule (drop it when the last descriptor closes)
is exactly the property a supervisor must not depend on, and it is invisible from
outside.

**An advisory lock also leaves nothing to report.** Reclamation needs a holder —
pid, when it wrote the record, why it can no longer be holding it — because the
reason a lock is left behind is usually the reason the publication before it did
not finish, and §14's flight recorder is where that belongs. A kernel lock has no
holder identity to hand back: whoever notices it deletes it and can say nothing
about whom.

**A pid is not an identity.** `/proc/sys/kernel/pid_max` on this machine is
4194304, so numbers recycle inside one boot, and a reboot recycles all of them at
once over a stale file. Two facts make a pid into an identity: `/proc/<pid>/stat`
field 22, the tick count at which the process began (`getconf CLK_TCK` = 100, so
hundredths of a second since boot), and `/proc/sys/kernel/random/boot_id`, which
says whether the record and this process share a boot at all.

**Measured: `kill(pid, 0)` reports a zombie as alive.** A `sleep` killed and left
unreaped still answers signal 0 with success, while the state letter in the same
stat line reads `Z`. A liveness test written with signal 0 alone keeps a lock alive
for a process that has stopped executing and will never delete a file — which, for
a supervisor whose children are reaped by whoever started them, is the ordinary
shape of "the holder died".

**Measured: `kill(1, 0)` as uid 1000 answers `EPERM`, and `kill(0, 0)` answers
Ok.** `EPERM` is an answer of "yes, that process exists, and it is not yours", not
an answer of "no": reclaiming on it would let any user's lock be deleted by any
other. Pid 0 is not a process at all — it names the caller's entire process group,
which for this tool is every gate and agent child it has started (ADR-0038), so a
record naming 0 must be refused before it is ever probed.

**A half-written record is the window that has no good threshold.** A lock created
empty and filled in afterwards has an interval in which it exists and says nothing,
and closing it needs an age rule ("a lock file older than N that cannot be read may
be taken"). Every N is wrong on some machine: too small turns a holder whose fsync
is slow into an abandoned one, which is two processes publishing at once. Measured:
`link(2)` into an existing name fails `EEXIST` (errno 17) while creating a new name
succeeds, and the draft's mode and content travel with the inode (a `0600` draft is
a `0600` lock file). An exclusive create and a complete record are therefore the
same step, with no threshold to argue about.

## Decision

`crates/ktask-core/src/lock.rs` holds one file, `repo.lock`, in the project's state
directory, created `0600`.

- **The claim is a hard link.** The record is written to a uniquely named draft
  (`repo.lock.pending-<token>`, `sync_all`ed) and then `fs::hard_link`ed into
  place. `AlreadyExists` is the answer that means "somebody else holds it"; every
  other failure is reported as it comes. The draft name is removed on every exit
  path, so an `acquire` leaves either the lock file or nothing.
- **The record is `{pid, started_ticks, boot_id, token, since}`**, as JSON, with
  unknown fields tolerated so a newer ktask-rs still names its holder to an older
  one. `started_ticks` and `boot_id` are optional: a machine that cannot answer is
  a machine that keeps the caller waiting, never one that hands over the lock.
- **Identity is pid *and* start tick *and* boot.** A lock is taken over only when
  the holder is provably gone: `ESRCH` from signal 0, or a live pid in state `Z`,
  or a live pid whose start tick differs from the one the record named
  (`PidReused`), or a record written in a different boot (`Rebooted`). `pid == 0`
  and a pid this platform cannot name are refused as unreadable records, not
  probed.
- **What cannot be proved abandoned is waited behind and never deleted**: a live
  holder, a pid answering `EPERM`, an oversized pid, a record that will not parse.
  Waiting is the safe side of a question this module cannot always answer; deleting
  is the side that turns one lock into two publishers.
- **Reclamation is a value, not a log line.** `RepoLock::reclaimed()` hands back
  `Reclaimed { path, pid, since, reason }` with a `Display` that says who left it
  and the ground for taking it, so the caller can journal it and the TUI can show
  it. The re-read immediately before the unlink is what keeps a takeover from
  deleting a record that changed while the verdict was being reached.
- **A release proves the file is still ours.** `release()` and `Drop` compare the
  `token` in the file to the one this holder wrote and refuse (`Error::Policy`)
  when it names another holder, is gone, or has gone unreadable.
- **Waiting is a 20 ms poll to the deadline**, and `Duration::ZERO` means look once
  and report — which is what `preflight`'s "lock acquired" wants. The timeout is
  `Error::Io` with `ErrorKind::TimedOut` so contention is distinguishable from a
  broken filesystem, and the message names the file, the holder's pid, and when
  that holder wrote the record.
- **The directory is never created**, as in `journal`: registration owns it
  (VISION.md §11), and an absent one is reported.

## Alternatives considered

- **`flock` / `nix::sys::file::flock`** — needs no staleness rule and is the
  reflex answer, and it loses on all three crash questions: measured to survive its
  own process through a forked child, held by every gate and agent child of
  ADR-0038, and silent about who held it, so the reclamation this task is required
  to report could not be reported at all.
- **`O_CREAT | O_EXCL` on the lock file, then write the record** — the half-written
  window above, and an age threshold to close it. The draft-plus-link form buys the
  same exclusivity with no threshold.
- **`mkdir` as the lock** — atomic and exclusive, and it has the same content
  window in a second file, plus a directory to clean up and still nothing to report.
- **`rename` as the claim** — atomic but *not* exclusive: it overwrites, so a second
  acquirer would replace the live holder's record instead of being refused.
- **A heartbeat or TTL on the record** (holder re-dates every N; a stale record is
  reclaimable) — trades a provable verdict for a guessed one, needs a timer in a
  tool with no async runtime, and any N picks a machine where it is too short.
  Start ticks and boot id answer the same question with facts instead.
- **Reclaiming when the record cannot be read, or when signal 0 says `EPERM`** —
  both delete a lock that may be live, and the cost is asymmetric: a wrong reclaim
  publishes two runs at once, a wrong wait times out and says who is in the way.
- **A pid-only staleness rule** — the standard `pid + mtime` idiom; it cannot tell a
  reused pid from its holder, and on this machine pids recycle within
  `pid_max` numbers.
- **Serializing on the journal's SQLite write lock** — a transaction cannot span the
  seconds a push takes, and one that dies is rolled back silently, which is the
  nothing-to-report property again. The journal records what happened; it is not
  the thing that keeps two runs out of each other.
- **A dedicated `Error::LockHeld { pid, since }` variant** — `docs/DESIGN.md` closes
  the enum at eleven variants and VISION.md §3 gives that decision to the human
  (ADR-0046 hit the same wall); `Error::Io` with `TimedOut` already separates
  contention from a broken filesystem, and the message carries the pid and instant.

## Consequences

- A run can say *why* it was allowed to publish over another's lock, in one
  sentence a reader can check by hand, instead of asserting that the pid looked
  old.
- The lock does not close a two-reclaimer race: two processes that both decide a
  lock is abandoned can both unlink and then both link, one winning the link and
  the other believing it holds the lock. It is left open deliberately —
  ADR-0046 decided publication is proven by the tip a fetch brought back, not by
  the lock held while pushing, so this lock's job is to make interference rare. If
  a later task needs the lock itself to be the proof, that needs a different
  mechanism (a kernel lock, or a claim the filesystem arbitrates atomically) and a
  superseding ADR.
- A holder this process cannot examine blocks it for the whole `timeout` and then
  times out naming it. That is the intended failure; the operator's answer is the
  pid in the message.
- Liveness is answered through `/proc` and `nix`, so the reclamation rules are
  Linux's. Elsewhere the file still serializes live processes on one machine, and
  nothing would ever be reclaimed: the `Option`s in the record are what that
  degradation costs, and it is why they are `Option` rather than required.
- Waiting costs one draft write with `sync_all` per 20 ms poll, so noticing a
  release is prompt and a long wait is a few hundred small writes rather than a
  spin. The retry loop is not bounded by the deadline: a lock freed in the last
  instant must still be takeable, and every retry pays for a synchronous write, so
  the loop cannot spin hot even against a file that keeps appearing and vanishing.
- The lock serializes processes on one machine. It is not a lease: two hosts
  sharing a filesystem over a network mount are not serialized by it, and no lock
  file would be without a lease protocol nobody is proposing here. §10's hazard is
  a second TUI on the box that holds the checkout.
- Draft names left by a crash (`repo.lock.pending-*`) are inert and are not swept.
  Nobody can claim them and the next `acquire` writes its own; a janitor task that
  tidies a state directory should include them.
