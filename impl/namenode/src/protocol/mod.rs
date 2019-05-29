use hdfs_protos::hadoop::hdfs::{DatanodeInfoProto, HdfsFileStatusProto, LocatedBlockProto, LocatedBlocksProto};
use radix::RadixQuery;

use crate::block::BlockStore;
use crate::datanode::{Datanode, DatanodeStore};
use crate::file::{File, FileStore};
use crate::index::Index;
use crate::storage::StorageStore;

mod client_namenode;
mod datanode;
mod nahfs;

pub use client_namenode::ClientNamenodeProtocol;
pub use datanode::DatanodeProtocol;
pub use nahfs::NahfsProtocol;

fn to_datanode_info_proto(datanode: &Datanode,
        storage_store: Option<&StorageStore>) -> DatanodeInfoProto {
    let mut last_update = 0;

    // iniitalize DatanodeInfoProto
    let mut din_proto = DatanodeInfoProto::default();
    din_proto.admin_state = Some(0); // NORMAL
    din_proto.location = Some("/default-rack".to_string());

    // populate DatanodeIdProto
    let mut di_proto = &mut din_proto.id;
    di_proto.ip_addr = datanode.ip_address.clone();
    di_proto.datanode_uuid = datanode.id.clone();
    di_proto.xfer_port = datanode.xfer_port;

    // populate storage state variables
    let (mut capacity, mut dfs_used, mut remaining,
         mut block_pool_used, mut non_dfs_used) = (0, 0, 0, 0, 0);
    if let Some(storage_store) = storage_store {
        for storage_id in &datanode.storage_ids {
            if let Some(storage) =
                    storage_store.get_storage(storage_id) {
                if let Some(state) = storage.states.last() {
                    capacity += state.capacity.unwrap_or(0);
                    dfs_used += state.dfs_used.unwrap_or(0);
                    remaining += state.remaining.unwrap_or(0);
                    block_pool_used += state.block_pool_used.unwrap_or(0);
                    non_dfs_used += state.non_dfs_used.unwrap_or(0);
                    last_update = std::cmp::max(last_update,
                        state.update_timestamp);
                }
            }
        }
    }

    din_proto.capacity = Some(capacity);
    din_proto.dfs_used = Some(dfs_used);
    din_proto.remaining = Some(remaining);
    din_proto.block_pool_used = Some(block_pool_used);
    din_proto.non_dfs_used = Some(non_dfs_used);

    // populate datanode state variables
    if let Some(state) = datanode.states.last() {
        din_proto.cache_capacity = state.cache_capacity;
        din_proto.cache_used = state.cache_used;
        din_proto.xceiver_count = state.xceiver_count;
        last_update = std::cmp::max(last_update,
            state.update_timestamp);
    }

    // last updated = max of most recent datanode and storage states
    if last_update != 0 {
        din_proto.last_update = Some(last_update);
    }
 
    din_proto
}

fn to_hdfs_file_status_proto(file: &File,
        query: &Option<(&str, RadixQuery)>, block_store: &BlockStore,
        file_store: &FileStore, index: &Index) -> HdfsFileStatusProto {
    let mut hfs_proto = HdfsFileStatusProto::default();
    hfs_proto.file_type = file.file_type;
    hfs_proto.path = file_store.compute_path(file.inode).into_bytes();
    if let Some((query_string, _)) = query {
        hfs_proto.path.push('+' as u8);

        for value in query_string.as_bytes() {
            hfs_proto.path.push(*value);
        }
    }

    // iterate over blocks to compute file length
    hfs_proto.length = 0;
    for (block_id, query_result) in validate_block_ids(
            &file.blocks, block_store, index, query) {
        match query_result {
            Some((_, length)) => hfs_proto.length += length as u64,
            None => {
                let block = block_store.get_block(&block_id).unwrap();
                hfs_proto.length += block.length;
            },
        }
    }

    let fp_proto = &mut hfs_proto.permission;
    fp_proto.perm = file.permissions;

    hfs_proto.owner = file.owner.clone();
    hfs_proto.group = file.group.clone();

    match file.file_type {
        1 =>  {
            if let Some(children) = file_store.get_children(file.inode) {
                hfs_proto.children_num = Some(children.len() as i32);
            }
        },
        2 => {
            hfs_proto.block_replication = Some(file.block_replication);
            hfs_proto.blocksize = Some(file.block_size);
        },
        _ => unimplemented!(),
    }

    // TODO - add locations if necessary
    hfs_proto.file_id = Some(file.inode);

    hfs_proto
}

fn to_located_blocks_proto(file: &File,
        query: &Option<(&str, RadixQuery)>, block_store: &BlockStore,
        datanode_store: &DatanodeStore, index: &Index,
        storage_store: &StorageStore) -> LocatedBlocksProto {
    let mut lbs_proto = LocatedBlocksProto::default();
    let blocks = &mut lbs_proto.blocks;

    let (mut length, complete) = (0, true);
    let valid_block_ids = validate_block_ids(&file.blocks,
        block_store, index, query);
    for (block_id, query_result) in valid_block_ids {
        let block = block_store.get_block(&block_id).unwrap();

        // populate LocatedBlockProto
        let mut lb_proto = LocatedBlockProto::default();
        let eb_proto = &mut lb_proto.b;

        // populate ExtendedBlockProto
        match query_result {
            Some((query_block_id, length)) => {
                eb_proto.block_id = query_block_id;
                eb_proto.num_bytes = Some(length as u64);
            },
            None => {
                eb_proto.block_id = block.id;
                eb_proto.num_bytes = Some(block.length);
            },
        }

        // populate LocatedBlockProto
        lb_proto.offset = length;
        lb_proto.corrupt = false;

        // populate locs
        for datanode_id in &block.locations {
            if let Some(datanode) =
                    datanode_store.get_datanode(datanode_id) {
                lb_proto.locs.push(to_datanode_info_proto(
                    datanode, Some(storage_store)));
            }
        }

        // populate storages
        for storage_id in &block.storage_ids {
            lb_proto.storage_types.push(0);
            lb_proto.storage_i_ds.push(storage_id.to_string());
            lb_proto.is_cached.push(false);
        }

        length += eb_proto.num_bytes.unwrap(); // increment file length
        blocks.push(lb_proto);
        //length += block.length;
        /*} else {
            // TODO - figure out if file is complete
            // block_id not found -> file not complete
            complete = false;
        }*/
    }

    lbs_proto.file_length = length;
    lbs_proto.under_construction = !complete;
    lbs_proto.is_last_block_complete = complete;
    lbs_proto
}

fn validate_block_ids(block_ids: &Vec<u64>, block_store: &BlockStore,
        index: &Index, query: &Option<(&str, RadixQuery)>)
        -> Vec<(u64, Option<(u64, u32)>)> {
    let mut blocks = Vec::new();

    match query {
        Some((_, query)) => {
            // submit query to index
            let query_map = index.query(query, block_ids);

            // iterate over length_map
            for (block_id, (geohashes, lengths)) in query_map.iter() {
                if geohashes.len() == 0 {
                    continue;
                }

                // compute block_id
                let query_block_id = shared::block
                    ::encode_block_id(&block_id, &geohashes);

                // compute block length
                let mut query_block_length = 0;
                for length in lengths {
                    query_block_length += length;
                }

                blocks.push((*block_id,
                    Some((query_block_id, query_block_length))));
            }
        },
        None => {
            // if no query -> return blocks that exist in BlockStore
            for block_id in block_ids {
                if let Some(_) = block_store.get_block(block_id) {
                    blocks.push((*block_id, None));
                }
            }
        },
    }

    blocks
}
