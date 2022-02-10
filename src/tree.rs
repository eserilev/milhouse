use crate::utils::{opt_packing_depth, opt_packing_factor};
use crate::{Arc, Error, Leaf, PackedLeaf};
use derivative::Derivative;
use eth2_hashing::{hash32_concat, ZERO_HASHES};
use parking_lot::RwLock;
use std::collections::BTreeMap;
use tree_hash::{Hash256, TreeHash};

#[derive(Debug, Derivative)]
#[derivative(PartialEq, Hash)]
pub enum Tree<T: TreeHash + Clone> {
    Leaf(Leaf<T>),
    PackedLeaf(PackedLeaf<T>),
    Node {
        #[derivative(PartialEq = "ignore", Hash = "ignore")]
        hash: RwLock<Hash256>,
        left: Arc<Self>,
        right: Arc<Self>,
    },
    Zero(usize),
}

impl<T: TreeHash + Clone> Clone for Tree<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Node { hash, left, right } => Self::Node {
                hash: RwLock::new(*hash.read()),
                left: left.clone(),
                right: right.clone(),
            },
            Self::Leaf(leaf) => Self::Leaf(leaf.clone()),
            Self::PackedLeaf(leaf) => Self::PackedLeaf(leaf.clone()),
            Self::Zero(depth) => Self::Zero(*depth),
        }
    }
}

impl<T: TreeHash + Clone> Tree<T> {
    pub fn empty(depth: usize) -> Arc<Self> {
        Self::zero(depth)
    }

    pub fn node(left: Arc<Self>, right: Arc<Self>) -> Arc<Self> {
        Arc::new(Self::Node {
            hash: RwLock::new(Hash256::zero()),
            left,
            right,
        })
    }

    pub fn zero(depth: usize) -> Arc<Self> {
        Arc::new(Self::Zero(depth))
    }

    pub fn leaf(value: T) -> Arc<Self> {
        Arc::new(Self::Leaf(Leaf::new(value)))
    }

    pub fn node_unboxed(left: Arc<Self>, right: Arc<Self>) -> Self {
        Self::Node {
            hash: RwLock::new(Hash256::zero()),
            left,
            right,
        }
    }

    pub fn zero_unboxed(depth: usize) -> Self {
        Self::Zero(depth)
    }

    pub fn leaf_unboxed(value: T) -> Self {
        Self::Leaf(Leaf::new(value))
    }

    pub fn get(&self, index: usize, depth: usize) -> Option<&T> {
        match self {
            Self::Leaf(Leaf { value, .. }) if depth == 0 => Some(value),
            Self::PackedLeaf(PackedLeaf { values, .. }) if depth == 0 => {
                values.get(index % T::tree_hash_packing_factor())
            }
            Self::Node { left, right, .. } if depth > 0 => {
                let packing_depth = opt_packing_depth::<T>().unwrap_or(0);
                let new_depth = depth - 1;
                // Left
                if (index >> (new_depth + packing_depth)) & 1 == 0 {
                    left.get(index, new_depth)
                }
                // Right
                else {
                    right.get(index, new_depth)
                }
            }
            _ => None,
        }
    }

    /// Create a new tree where the `index`th leaf is set to `new_value`.
    ///
    /// NOTE: callers are responsible for bounds-checking `index` before calling this function.
    pub fn with_updated_leaf(
        &self,
        index: usize,
        new_value: T,
        depth: usize,
    ) -> Result<Arc<Self>, Error> {
        match self {
            Self::Leaf(_) if depth == 0 => Ok(Self::leaf(new_value)),
            Self::PackedLeaf(leaf) if depth == 0 => Ok(Arc::new(Self::PackedLeaf(
                leaf.insert_at_index(index, new_value)?,
            ))),
            Self::Node { left, right, .. } if depth > 0 => {
                let packing_depth = opt_packing_depth::<T>().unwrap_or(0);
                let new_depth = depth - 1;
                if (index >> (new_depth + packing_depth)) & 1 == 0 {
                    // Index lies on the left, recurse left
                    Ok(Self::node(
                        left.with_updated_leaf(index, new_value, new_depth)?,
                        right.clone(),
                    ))
                } else {
                    // Index lies on the right, recurse right
                    Ok(Self::node(
                        left.clone(),
                        right.with_updated_leaf(index, new_value, new_depth)?,
                    ))
                }
            }
            Self::Zero(zero_depth) if *zero_depth == depth => {
                if depth == 0 {
                    if opt_packing_factor::<T>().is_some() {
                        Ok(Arc::new(Self::PackedLeaf(PackedLeaf::single(new_value))))
                    } else {
                        Ok(Self::leaf(new_value))
                    }
                } else {
                    // Split zero node into a node with left and right, and recurse into
                    // the appropriate subtree
                    let new_zero = Self::zero(depth - 1);
                    Self::node(new_zero.clone(), new_zero)
                        .with_updated_leaf(index, new_value, depth)
                }
            }
            _ => Err(Error::UpdateLeafError),
        }
    }

    pub fn with_updated_leaves(
        &self,
        mut updates: BTreeMap<usize, T>,
        prefix: usize,
        depth: usize,
    ) -> Result<Arc<Self>, Error> {
        match self {
            Self::Leaf(_) if depth == 0 => {
                let index = prefix;
                let value = updates
                    .remove(&index)
                    .ok_or(Error::LeafUpdateMissing { index })?;
                Ok(Self::leaf(value))
            }
            Self::PackedLeaf(packed_leaf) if depth == 0 => Ok(Arc::new(Self::PackedLeaf(
                packed_leaf.update(prefix, updates)?,
            ))),
            Self::Node { left, right, .. } if depth > 0 => {
                if updates.is_empty() {
                    return Err(Error::NodeUpdatesMissing { prefix });
                }

                let packing_depth = opt_packing_depth::<T>().unwrap_or(0);
                let new_depth = depth - 1;
                let left_prefix = prefix;
                let right_prefix = prefix | (1 << (new_depth + packing_depth));

                let right_updates = updates.split_off(&right_prefix);
                let left_updates = updates;

                let new_left = if !left_updates.is_empty() {
                    left.with_updated_leaves(left_updates, left_prefix, new_depth)?
                } else {
                    left.clone()
                };
                let new_right = if !right_updates.is_empty() {
                    right.with_updated_leaves(right_updates, right_prefix, new_depth)?
                } else {
                    right.clone()
                };
                Ok(Self::node(new_left, new_right))
            }
            Self::Zero(zero_depth) if *zero_depth == depth => {
                if depth == 0 {
                    if opt_packing_factor::<T>().is_some() {
                        let packed_leaf = PackedLeaf::empty().update(prefix, updates)?;
                        Ok(Arc::new(Self::PackedLeaf(packed_leaf)))
                    } else {
                        let index = prefix;
                        let value = updates
                            .remove(&index)
                            .ok_or(Error::LeafUpdateMissing { index })?;
                        Ok(Self::leaf(value))
                    }
                } else {
                    // Split zero node into a node with left and right and recurse.
                    let new_zero = Self::zero(depth - 1);
                    Self::node(new_zero.clone(), new_zero)
                        .with_updated_leaves(updates, prefix, depth)
                }
            }
            _ => Err(Error::UpdateLeavesError),
        }
    }
}

impl<T: TreeHash + Clone + Send + Sync> Tree<T> {
    pub fn tree_hash(&self) -> Hash256 {
        match self {
            Self::Leaf(Leaf { hash, value }) => {
                // FIXME(sproul): upgradeable RwLock?
                let read_lock = hash.read();
                let existing_hash = *read_lock;
                drop(read_lock);
                // FIXME(sproul): re-consider 0 leaf case performance
                if !existing_hash.is_zero() {
                    existing_hash
                } else {
                    let tree_hash = value.tree_hash_root();
                    *hash.write() = tree_hash;
                    tree_hash
                }
            }
            Self::PackedLeaf(leaf) => leaf.tree_hash(),
            Self::Zero(depth) => Hash256::from_slice(&ZERO_HASHES[*depth]),
            Self::Node { hash, left, right } => {
                let read_lock = hash.read();
                let existing_hash = *read_lock;
                drop(read_lock);

                if !existing_hash.is_zero() {
                    existing_hash
                } else {
                    // Parallelism goes brrrr.
                    let (left_hash, right_hash) =
                        rayon::join(|| left.tree_hash(), || right.tree_hash());
                    let tree_hash =
                        Hash256::from(hash32_concat(left_hash.as_bytes(), right_hash.as_bytes()));
                    *hash.write() = tree_hash;
                    tree_hash
                }
            }
        }
    }
}
