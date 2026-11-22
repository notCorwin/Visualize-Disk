//! Isolated on-disk sample data for the explicitly requested demo session.
use crate::scan::{FileIdentity, is_link_or_reparse};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub struct DemoRoot {
    pub path: PathBuf,
    real_path: PathBuf,
    identity: FileIdentity,
}

impl DemoRoot {
    pub fn scan(
        &self,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
        progress: std::sync::Arc<std::sync::Mutex<crate::scan::ScanProgress>>,
    ) -> Result<crate::scan::ScanResult, String> {
        self.validate()?;
        let result = crate::scan::scan_restricted(
            self.path.clone(),
            &self.real_path,
            self.identity,
            cancel,
            progress,
        )?;
        self.validate()?;
        Ok(result)
    }
    pub fn create() -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        let parent = crate::file_actions::home_directory()?.join("Library/Caches");
        #[cfg(not(target_os = "macos"))]
        let parent = std::env::temp_dir();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "无法创建演示数据，请检查系统时间。")?
            .as_nanos();
        let path = parent.join(format!("Visualize-SSD-Demo-{}-{stamp}", std::process::id()));
        // Never reuse or replace a pre-existing directory, even on failure.
        fs::create_dir(&path).map_err(|_| "无法创建演示目录，请检查个人缓存目录权限。")?;
        let metadata = fs::symlink_metadata(&path).map_err(|_| "无法确认演示目录。")?;
        let identity =
            FileIdentity::from_path(&path, &metadata).map_err(|_| "无法确认演示目录身份。")?;
        let real_path = fs::canonicalize(&path).map_err(|_| "无法确认演示目录位置。")?;
        let root = Self {
            path,
            real_path,
            identity,
        };
        root.populate()
            .map_err(|_| "演示数据创建失败，请检查个人缓存目录的可用空间和权限。")?;
        Ok(root)
    }

    fn populate(&self) -> std::io::Result<()> {
        for group in 0..32u64 {
            let folder = self.path.join(format!("示例文件夹-{group:02}"));
            fs::create_dir(&folder)?;
            // Some folders have children and direct files, exercising all views.
            if group % 4 == 0 {
                let child = folder.join("子文件夹");
                fs::create_dir(&child)?;
                for index in 0..12u64 {
                    create_sample(&child, group * 31 + index)?;
                }
            }
            if group % 8 == 0 {
                fs::create_dir(folder.join("空文件夹"))?;
            }
            for index in 0..24u64 {
                create_sample(&folder, group * 31 + index)?;
            }
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        let meta =
            fs::symlink_metadata(&self.path).map_err(|_| "演示目录已移动或删除，请重置演示。")?;
        if !meta.is_dir()
            || is_link_or_reparse(&meta)
            || FileIdentity::from_path(&self.path, &meta).ok() != Some(self.identity)
            || fs::canonicalize(&self.path).ok().as_deref() != Some(self.real_path.as_path())
        {
            return Err("演示目录已更改，请重置演示。".into());
        }
        Ok(())
    }

    pub fn allow_target(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        if path == self.path || !path.starts_with(&self.path) {
            return Err("演示模式只能操作演示目录中的文件。".into());
        }
        let actual = fs::canonicalize(path).map_err(|_| "演示文件已移动或删除，请重新扫描。")?;
        if actual == self.real_path || !actual.starts_with(&self.real_path) {
            return Err("演示模式只能操作演示目录中的文件。".into());
        }
        Ok(())
    }
}

fn create_sample(folder: &Path, index: u64) -> std::io::Result<()> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(folder.join(format!("示例文件-{index:04}.dat")))?;
    // Sparse files provide useful logical sizes without writing gigabytes.
    #[cfg(target_os = "macos")]
    let size = (1 + (index * 73 % 256)) * 256 * 1024;
    #[cfg(not(target_os = "macos"))]
    let size = (1 + (index * 73 % 16)) * 4096;
    file.set_len(size)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct TrashOnDrop(PathBuf);
    impl Drop for TrashOnDrop {
        fn drop(&mut self) {
            let _ = std::process::Command::new("/usr/bin/trash")
                .arg(&self.0)
                .status();
        }
    }

    #[test]
    fn demo_guard_rejects_root_external_and_symlink_escape() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let parent = std::env::temp_dir().join(format!(
            "visualize-demo-guard-{}-{stamp}",
            std::process::id()
        ));
        let path = parent.join("demo");
        let outside = parent.join("outside");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&path).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(path.join("sample"), b"sample").unwrap();
        fs::write(outside.join("real"), b"real").unwrap();
        symlink(&outside, path.join("escape")).unwrap();
        let meta = fs::symlink_metadata(&path).unwrap();
        let root = DemoRoot {
            identity: FileIdentity::from_path(&path, &meta).unwrap(),
            real_path: fs::canonicalize(&path).unwrap(),
            path: path.clone(),
        };
        assert!(root.allow_target(&path.join("sample")).is_ok());
        assert!(root.allow_target(&path).is_err());
        assert!(root.allow_target(&outside.join("real")).is_err());
        assert!(root.allow_target(&path.join("escape/real")).is_err());
        // Simulate a root being replaced during a Finder authorization prompt.
        // The moved target still has the same file identity, but it is no
        // longer inside the originally verified demo root.
        let sample = path.join("sample");
        let metadata = fs::symlink_metadata(&sample).unwrap();
        let sample_id = FileIdentity::from_path(&sample, &metadata).unwrap();
        fs::rename(&path, outside.join("moved")).unwrap();
        symlink(outside.join("moved"), &path).unwrap();
        assert!(
            crate::file_actions::verify_target(
                &sample,
                sample_id,
                crate::file_actions::TargetKind::File
            )
            .is_ok()
        );
        assert!(
            crate::file_actions::verify_before_dispatch(
                &sample,
                sample_id,
                crate::file_actions::TargetKind::File,
                &|| root.allow_target(&sample)
            )
            .is_err()
        );
        assert!(
            root.scan(
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                std::sync::Arc::new(std::sync::Mutex::new(Default::default()))
            )
            .is_err()
        );
        // Only move the directory created by this test, never delete user data.
        let _cleanup = TrashOnDrop(parent);
    }

    #[test]
    fn demo_fixture_scans_without_touching_external_files() {
        use std::{
            os::unix::fs::MetadataExt,
            sync::{Arc, Mutex, atomic::AtomicBool},
        };
        let root = DemoRoot::create().unwrap();
        let _cleanup = TrashOnDrop(root.path.clone());
        let result = root
            .scan(
                Arc::new(AtomicBool::new(false)),
                Arc::new(Mutex::new(Default::default())),
            )
            .unwrap();
        assert_eq!(result.files.len(), 864);
        assert!(
            result
                .files
                .iter()
                .all(|entry| entry.path.starts_with(&root.path)
                    && root.allow_target(&entry.path).is_ok())
        );
        let allocated: u64 = result
            .files
            .iter()
            .map(|entry| fs::metadata(&entry.path).unwrap().blocks() * 512)
            .sum();
        assert!(
            allocated < 100 * 1024 * 1024,
            "demo samples allocate {allocated} bytes"
        );
    }
}
