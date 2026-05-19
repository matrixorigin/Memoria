pub mod service;
pub use service::{
    classify_diff_rows, ApplyResult, ApplySelection, ApplyUpdatePair, ClassifiedDiff, DiffConflict,
    DiffConflictSide, DiffItem, DiffRow, DiffUpdatedPair, GitForDataService, Snapshot,
};
