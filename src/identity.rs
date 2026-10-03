//! This machine's RemSound instance id, kept across restarts: Windows remembers devices by it.

use std::path::Path;

use anyhow::Context;
use uuid::Uuid;

const FILE_NAME: &str = "instance-id";

/// Read the id from `state_dir`, creating and saving a new one the first time.
pub fn load_or_create(state_dir: &Path) -> anyhow::Result<(Uuid, bool)> {
    let path = state_dir.join(FILE_NAME);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let id = Uuid::parse_str(text.trim())
                .with_context(|| format!("{} does not hold a valid id", path.display()))?;
            anyhow::ensure!(!id.is_nil(), "{} holds the all-zero id", path.display());
            Ok((id, false))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(state_dir).with_context(|| {
                format!("cannot create the state directory {}", state_dir.display())
            })?;
            let id = Uuid::new_v4();
            let tmp = state_dir.join(format!("{FILE_NAME}.tmp"));
            std::fs::write(&tmp, format!("{id}\n"))
                .with_context(|| format!("cannot write {}", tmp.display()))?;
            std::fs::rename(&tmp, &path)
                .with_context(|| format!("cannot write {}", path.display()))?;
            Ok((id, true))
        }
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_stable_across_loads() {
        let dir = tempfile::tempdir().unwrap();
        let (a, created) = load_or_create(dir.path()).unwrap();
        assert!(created);
        let (b, created) = load_or_create(dir.path()).unwrap();
        assert!(!created);
        assert_eq!(a, b);
        std::fs::write(dir.path().join(FILE_NAME), "garbage").unwrap();
        assert!(load_or_create(dir.path()).is_err());
    }
}
