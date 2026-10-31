//! Walking a tree around excluded subtrees without asking the backend to filter.
//!
//! rclone drops pCloud's one-call recursive listing as soon as any directory filter is active
//! (phase 0: Immich took 133 s instead of 6 s), so exclusions are done by splitting the tree:
//! every folder with an excluded path somewhere below it is listed one level at a time, and
//! every other subtree is walked whole, with nothing excluded. Excluded subtrees are never
//! fetched at all.

use std::future::Future;
use std::pin::Pin;

use crate::policy::{CheckedPath, PathSet};
use crate::storage::{Entry, Storage, StorageError};

/// Everything under `root` except what is at or under a path in `exclude`, whose own folder
/// entries are kept (see [`Storage::walk`]). For backends: call it from `walk` when `exclude`
/// has something under `root`; it calls back into `walk` only with nothing to exclude.
pub async fn split_walk<S: Storage + ?Sized>(
    storage: &S,
    root: &CheckedPath,
    exclude: &[CheckedPath],
) -> Result<Vec<Entry>, StorageError> {
    let exclude = PathSet::new(exclude.iter().map(CheckedPath::as_str))?;
    let mut out = Vec::new();
    split_into(storage, root, &exclude, &mut out).await?;
    Ok(out)
}

fn split_into<'a, S: Storage + ?Sized>(
    storage: &'a S,
    dir: &'a CheckedPath,
    exclude: &'a PathSet,
    out: &'a mut Vec<Entry>,
) -> Pin<Box<dyn Future<Output = Result<(), StorageError>> + Send + 'a>> {
    Box::pin(async move {
        if exclude.covers(dir.as_str()) {
            return Ok(());
        }
        if !exclude.any_below(dir.as_str()) {
            out.extend(storage.walk(dir, &[]).await?);
            return Ok(());
        }
        let children = storage.list(dir).await?;
        for child in children {
            let path = dir.child(&child.name);
            let descend = child.is_dir && !exclude.covers(path.as_str());
            out.push(child);
            if descend {
                split_into(storage, &path, exclude, out).await?;
            }
        }
        Ok(())
    })
}
