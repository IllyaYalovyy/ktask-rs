//! Recovery of evidence pins left by earlier interactive remediation: retention retires
//! idle pins and their guard files before it deletes evidence.
use super::*;
use nix::{
    errno::Errno,
    fcntl::{OFlag, openat},
    sys::stat::Mode,
    unistd::{UnlinkatFlags, unlinkat},
};
use std::{
    fs::{self, File, TryLockError},
    io::{Read, Write},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::PathBuf,
};

/// A guardian's reservation marker: a pin whose guard still holds it was never claimed.
const GUARDIAN_RESERVED: &[u8] = b"pending guardian\n";

fn identity(file: &File) -> Result<String> {
    let metadata = file.metadata()?;
    Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
}

fn descriptor_path(file: &File) -> PathBuf {
    PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        file.as_raw_fd()
    ))
}

fn directory(root: &File) -> Result<File> {
    openat(
        root,
        "evidence-pins",
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(io)
}

// This durable ownership record outlives individual pins, so an empty pin table
// never authorizes sweeping a substituted directory's apparently orphaned files.
fn owned_directory(root: &File, directory: &File, create: bool) -> Result<bool> {
    let flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
    let expected = identity(directory)?;
    match openat(root, "evidence-pins.identity", flags, Mode::empty()) {
        Ok(fd) => {
            let file = File::from(fd);
            require(
                file.metadata()?.is_file(),
                "Evidence directory identity is not a regular file",
            )?;
            let mut actual = String::new();
            file.take(128).read_to_string(&mut actual)?;
            require(
                actual == expected,
                "Evidence guard directory changed; cleanup stopped",
            )?;
            Ok(true)
        }
        Err(Errno::ENOENT) if create => {
            require(
                fs::read_dir(descriptor_path(directory))?.next().is_none(),
                "Unowned evidence guard directory is not empty",
            )?;
            let mut file = File::from(
                openat(
                    root,
                    "evidence-pins.identity",
                    OFlag::O_WRONLY
                        | OFlag::O_CREAT
                        | OFlag::O_EXCL
                        | OFlag::O_NOFOLLOW
                        | OFlag::O_CLOEXEC,
                    Mode::S_IRUSR | Mode::S_IWUSR,
                )
                .map_err(io)?,
            );
            file.write_all(expected.as_bytes())?;
            file.sync_all()?;
            root.sync_all()?;
            Ok(true)
        }
        Err(Errno::ENOENT) => Ok(false),
        Err(error) => Err(io(error)),
    }
}

fn open(directory: &File, id: &str, role: &str, create: bool) -> Result<File> {
    let mut flags = OFlag::O_RDWR | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
    if create {
        flags |= OFlag::O_CREAT | OFlag::O_EXCL;
    }
    let file = File::from(
        openat(
            directory,
            format!("{id}.{role}").as_str(),
            flags,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )
        .map_err(io)?,
    );
    require(
        file.metadata()?.is_file(),
        "Evidence guard is not a regular file",
    )?;
    Ok(file)
}

fn idle(file: &File) -> Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

// An owner that died between reserving and spawning leaves the reservation behind.
// Call only while holding the owner and guardian locks: revoking it then stops a
// delayed guardian from claiming, so the pin becomes retirable.
fn unclaimed(guardian: &File) -> Result<bool> {
    let mut marker = Vec::new();
    guardian
        .take(GUARDIAN_RESERVED.len() as u64 + 1)
        .read_to_end(&mut marker)?;
    if marker == GUARDIAN_RESERVED {
        guardian.set_len(0)?;
        guardian.sync_all()?;
    }
    Ok(marker.is_empty() || marker == GUARDIAN_RESERVED)
}

fn verify(directory: &File, id: &str, role: &str, expected: &str) -> Result<File> {
    let file = open(directory, id, role, false)?;
    require(
        identity(&file)? == expected,
        "Evidence guard identity changed; cleanup stopped",
    )?;
    Ok(file)
}

// Remove only after the pin deletion commits. A crash leaves recoverable orphans,
// never a durable row whose missing lock is mistaken for proof of idleness.
fn remove(directory: &File, id: &str) -> Result<()> {
    for role in ["guardian", "owner"] {
        match unlinkat(
            directory,
            format!("{id}.{role}").as_str(),
            UnlinkatFlags::NoRemoveDir,
        ) {
            Ok(()) | Err(Errno::ENOENT) => {}
            Err(error) => return Err(io(error)),
        }
    }
    directory.sync_all()?;
    Ok(())
}

/// Retire idle pins and remove orphaned guard files. The checks run on guard locks, not
/// on the database, so no write transaction is open meanwhile; one short transaction
/// deletes the retired rows, and their files go only after it commits. A crash leaves
/// recoverable orphans, never a row whose missing lock is mistaken for idleness.
pub(super) fn recover(db: &rusqlite::Connection, root: &File) -> Result<()> {
    let pins = db
        .prepare(
            "SELECT id,directory_identity,owner_identity,guardian_identity FROM evidence_pins",
        )?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let directory = match directory(root) {
        Ok(directory) => directory,
        Err(Error::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound && pins.is_empty() =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if !owned_directory(root, &directory, false)? {
        require(
            pins.is_empty(),
            "Evidence directory ownership is missing; cleanup stopped",
        )?;
        return Ok(());
    }
    for (_, expected, _, _) in &pins {
        require(
            identity(&directory)? == *expected,
            "Evidence guard directory changed; cleanup stopped",
        )?;
    }
    // Guard files without a row read above are orphans of our original directory, unless
    // their owner holds a lock: a pin being recorded right now is never idle.
    let live_ids: std::collections::BTreeSet<_> = pins.iter().map(|pin| pin.0.as_str()).collect();
    let mut orphans = std::collections::BTreeSet::new();
    for entry in fs::read_dir(descriptor_path(&directory))? {
        let name = entry?.file_name();
        if let Some(id) = name.to_str().and_then(|s| {
            s.strip_suffix(".owner")
                .or_else(|| s.strip_suffix(".guardian"))
        }) && id.len() == 32
            && id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            && !live_ids.contains(id)
        {
            orphans.insert(id.to_owned());
        }
    }
    for id in orphans {
        let mut guards = Vec::new();
        let mut safe = true;
        for role in ["owner", "guardian"] {
            match open(&directory, &id, role, false) {
                Ok(file) => {
                    safe &= idle(&file)? && (role != "guardian" || file.metadata()?.len() == 0);
                    guards.push(file);
                }
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        if safe {
            remove(&directory, &id)?;
        }
    }
    // Held locks keep each retired pin idle until its files are removed.
    let mut retired = Vec::new();
    for (id, directory_identity, owner_identity, guardian_identity) in pins {
        require(
            identity(&directory)? == directory_identity,
            "Evidence guard directory changed; cleanup stopped",
        )?;
        let owner = verify(&directory, &id, "owner", &owner_identity)?;
        let guardian = verify(&directory, &id, "guardian", &guardian_identity)?;
        if idle(&owner)? && idle(&guardian)? && unclaimed(&guardian)? {
            retired.push((id, owner, guardian));
        }
    }
    if retired.is_empty() {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(db, rusqlite::TransactionBehavior::Immediate)?;
    for (id, _, _) in &retired {
        tx.execute("DELETE FROM evidence_pins WHERE id=?1", [id])?;
    }
    tx.commit()?;
    for (id, _owner, _guardian) in &retired {
        remove(&directory, id)?;
    }
    Ok(())
}
