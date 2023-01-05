use std::{
    collections::{BTreeMap, HashMap},
    fmt::{Display, Write},
    rc::Rc,
    sync::Arc,
};

use blake3::Hasher;
use chrono::{Duration, DurationRound, NaiveDateTime};
use models::{schema::ColumnType, utils, ColumnId, FieldId, Timestamp};
use parking_lot::RwLock;
use snafu::ResultExt;
use trace::warn;

use crate::{
    compaction::{CompactIterator, CompactingBlock},
    database::Database,
    error::{self, Error, Result},
    schema::schemas::DBschemas,
    tseries_family::{ColumnFile, TseriesFamily},
    tsm::{DataBlock, TsmReader},
    TimeRange, TseriesFamilyId,
};

pub type Hash = [u8; 32];

pub fn hash_to_string(hash: Hash) -> String {
    let mut s = String::with_capacity(32);
    for v in hash {
        s.push_str(format!("{:x}", v).as_str());
    }
    s
}

#[derive(Default, Debug)]
pub struct TableHashTreeNode {
    table: String,
    columns: Vec<ColumnHashTreeNode>,
}

impl TableHashTreeNode {
    pub fn with_capacity(table: String, capacity: usize) -> Self {
        Self {
            table,
            columns: Vec::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, value: ColumnHashTreeNode) {
        self.columns.push(value);
    }
}

impl Display for TableHashTreeNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{{ table: {}, values: [ ", self.table))?;
        for v in self.columns.iter() {
            v.fmt(f)?;
            f.write_str(", ")?;
        }
        f.write_str("] }")?;
        Ok(())
    }
}

#[derive(Default, Debug)]
pub struct ColumnHashTreeNode {
    column: String,
    values: Vec<TimeRangeHashTreeNode>,
}

impl ColumnHashTreeNode {
    pub fn with_capacity(column: String, capacity: usize) -> Self {
        Self {
            column,
            values: Vec::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, value: TimeRangeHashTreeNode) {
        self.values.push(value);
    }
}

impl Display for ColumnHashTreeNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{{ column: {}, values: [ ", self.column))?;
        for v in self.values.iter() {
            v.fmt(f)?;
            f.write_str(", ")?;
        }
        f.write_str("] }")?;
        Ok(())
    }
}

#[derive(Default, Debug)]
pub struct TimeRangeHashTreeNode {
    min_ts: Timestamp,
    max_ts: Timestamp,
    hash: Hash,
}

impl TimeRangeHashTreeNode {
    pub fn new(time_range: TimeRange, hash: Hash) -> Self {
        Self {
            min_ts: time_range.min_ts,
            max_ts: time_range.max_ts,
            hash,
        }
    }
}

impl Display for TimeRangeHashTreeNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "{{ time_range: ({}, {}), hash: ",
            self.min_ts, self.max_ts
        ))?;
        for v in self.hash {
            f.write_fmt(format_args!("{:x}", v))?;
        }
        f.write_char('}')?;
        Ok(())
    }
}

pub(crate) async fn get_ts_family_hash_tree(
    database: &Database,
    ts_family_id: TseriesFamilyId,
) -> Result<Vec<TableHashTreeNode>> {
    const MAX_DATA_BLOCK_SIZE: u32 = 1000;

    let ts_family = match database.get_tsfamily(ts_family_id) {
        Some(t) => t,
        None => {
            return Err(Error::InvalidParam {
                reason: format!("can not find ts_family '{}'", ts_family_id),
            })
        }
    };

    let schemas = database.get_schemas();
    let mut cid_table_name_map: HashMap<ColumnId, Rc<String>> = HashMap::new();
    let mut cid_col_name_map: HashMap<ColumnId, String> = HashMap::new();
    for tab in schemas.list_tables().context(error::SchemaSnafu)? {
        match schemas.get_table_schema(&tab).context(error::SchemaSnafu)? {
            Some(sch) => {
                let shared_tab = Rc::new(tab);
                for col in sch.columns() {
                    if col.column_type != ColumnType::Time && col.column_type != ColumnType::Tag {
                        cid_table_name_map.insert(col.id, shared_tab.clone());
                        cid_col_name_map.insert(col.id, col.name.clone());
                    }
                }
            }
            None => {
                warn!("Repair: Can not find schema for table '{}'.", &tab);
            }
        }
    }
    let time_range_nanosec = get_default_time_range(schemas)?;

    // let ts_family_rlock = ts_family.read();
    // let version = ts_family_rlock.version();
    // let ts_family_id = ts_family_rlock.tf_id();
    // drop(ts_family_rlock);
    let (version, ts_family_id) = {
        let ts_family_rlock = ts_family.read().await;
        (ts_family_rlock.version(), ts_family_rlock.tf_id())
    };
    let mut readers: Vec<TsmReader> = Vec::new();
    for path in version
        .levels_info()
        .iter()
        .flat_map(|l| l.files.iter().map(|f| f.file_path()))
    {
        let r = TsmReader::open(path).await?;
        readers.push(r);
    }

    // Build a compact iterator, read data, slite by time range and then calculate hash.
    let iter = CompactIterator::new(readers, MAX_DATA_BLOCK_SIZE, true);
    let (fid_tr_hash_val_map, mut cid_fid_count_map) =
        read_from_compact_iterator(iter, ts_family_id, time_range_nanosec, &cid_col_name_map)
            .await?;

    // Transform hashed data into TableHashTreeNode list.
    let mut cid_tr_hasher_map: HashMap<ColumnId, BTreeMap<TimeRange, Hasher>> = HashMap::new();
    let mut hash_tree_builder: HashMap<String, TableHashTreeNode> = HashMap::new();
    for (fid, tr_hashes) in fid_tr_hash_val_map.into_iter() {
        if tr_hashes.is_empty() {
            continue;
        }
        let (column_id, _) = utils::split_id(fid);
        if !cid_col_name_map.contains_key(&column_id) {
            continue;
        }
        let tr_hasher_map = cid_tr_hasher_map.entry(column_id).or_default();
        for (tr, hash) in tr_hashes.into_iter() {
            tr_hasher_map.entry(tr).or_default().update(&hash);
        }
        if let Some(fid_count) = cid_fid_count_map.get_mut(&column_id) {
            *fid_count -= 1;
            if *fid_count == 0 {
                let table_name = cid_table_name_map.get(&column_id).unwrap();
                let table_node = match hash_tree_builder.get_mut(table_name.as_ref()) {
                    Some(t) => t,
                    None => hash_tree_builder
                        .entry((**table_name).clone())
                        .or_insert_with(|| {
                            TableHashTreeNode::with_capacity(
                                (**table_name).clone(),
                                cid_fid_count_map.len(),
                            )
                        }),
                };

                // cid_col_name_map must contains key column_id
                let column_name = cid_col_name_map.remove(&column_id).unwrap();
                // tr_hasher_map must contains key column_id
                let tr_hasher_map = cid_tr_hasher_map.remove(&column_id).unwrap();
                let mut column_node =
                    ColumnHashTreeNode::with_capacity(column_name, tr_hasher_map.len());
                for (tr, hasher) in tr_hasher_map.into_iter() {
                    column_node.push(TimeRangeHashTreeNode::new(tr, hasher.finalize().into()));
                }
                table_node.push(column_node);
            }
        };
    }

    Ok(hash_tree_builder
        .into_iter()
        .map(|(k, v)| v)
        .collect::<Vec<TableHashTreeNode>>())
}

fn get_default_time_range(db_schemas: Arc<DBschemas>) -> Result<i64> {
    let db_schema = db_schemas.db_schema().context(error::SchemaSnafu)?;
    let tenant_name = db_schema.tenant_name();
    let database_name = db_schema.database_name();
    Ok(db_schema
        .config
        .ttl()
        .as_ref()
        .map(|t| t.to_nanoseconds() / 1000)
        .unwrap_or(5 * 60 * 1_000_000_000))
}

fn calc_block_partial_time_range(
    timestamp: Timestamp,
    time_range: Duration,
    time_range_nanosecs: i64,
) -> Result<(Timestamp, Timestamp)> {
    let secs: i64 = timestamp / 1_000_000_000;
    let nsecs: u32 = (timestamp % 1_000_000_000) as u32;
    match NaiveDateTime::from_timestamp_opt(secs, nsecs) {
        Some(datetime) => {
            let min_ts = match datetime.duration_trunc(time_range) {
                Ok(date_time) => date_time.timestamp_nanos(),
                Err(e) => {
                    return Err(Error::Transform {
                        reason: format!("error truncing timestamp {}: {:?}", datetime, e),
                    })
                }
            };
            let max_ts = min_ts + time_range_nanosecs;
            Ok((min_ts, max_ts))
        }
        None => Err(Error::Transform {
            reason: format!("error parsing timestamp to NaiveDateTime: {}", timestamp),
        }),
    }
}

fn find_timestamp(timestamps: &[Timestamp], max_timestamp: Timestamp) -> usize {
    if max_timestamp != Timestamp::MIN {
        match timestamps.binary_search(&max_timestamp) {
            Ok(i) => i,
            Err(i) => i,
        }
    } else {
        timestamps.len()
    }
}

fn hash_partial_datablock(
    hasher: &mut Hasher,
    data_block: &DataBlock,
    min_idx: usize,
    max_timestamp: Timestamp,
) -> usize {
    match data_block {
        DataBlock::U64 { ts, val, .. } => {
            let limit = min_idx + find_timestamp(&ts[min_idx..], max_timestamp);
            for (i, v) in val.iter().enumerate().skip(min_idx).take(limit) {
                hasher.update(v.to_be_bytes().as_slice());
            }
            limit
        }
        DataBlock::I64 { ts, val, .. } => {
            let limit = min_idx + find_timestamp(&ts[min_idx..], max_timestamp);
            for (i, v) in val.iter().enumerate().skip(min_idx).take(limit) {
                hasher.update(v.to_be_bytes().as_slice());
            }
            limit
        }
        DataBlock::F64 { ts, val, .. } => {
            let limit = min_idx + find_timestamp(&ts[min_idx..], max_timestamp);
            for (i, v) in val.iter().enumerate().skip(min_idx).take(limit) {
                hasher.update(v.to_be_bytes().as_slice());
            }
            limit
        }
        DataBlock::Str { ts, val, .. } => {
            let limit = min_idx + find_timestamp(&ts[min_idx..], max_timestamp);
            for (i, v) in val.iter().enumerate().skip(min_idx).take(limit) {
                hasher.update(v.as_slice());
            }
            limit
        }
        DataBlock::Bool { ts, val, .. } => {
            let limit = min_idx + find_timestamp(&ts[min_idx..], max_timestamp);
            for (i, v) in val.iter().enumerate().skip(min_idx).take(limit) {
                hasher.update(if *v { &[1_u8] } else { &[0_u8] });
            }
            limit
        }
    }
}

async fn read_from_compact_iterator(
    mut iter: CompactIterator,
    ts_family_id: TseriesFamilyId,
    time_range_nanosec: i64,
    cid_col_name_map: &HashMap<ColumnId, String>,
) -> Result<(
    HashMap<FieldId, Vec<(TimeRange, Hash)>>,
    HashMap<ColumnId, usize>,
)> {
    let time_range = Duration::nanoseconds(time_range_nanosec);
    let mut fid_tr_hash_val_map: HashMap<FieldId, Vec<(TimeRange, Hash)>> = HashMap::new();
    let mut cid_fid_count_map: HashMap<ColumnId, usize> = HashMap::new();
    let mut last_hashed_tr_fid: Option<(TimeRange, FieldId)> = None;
    let mut hasher = Hasher::new();
    loop {
        match iter.next().await {
            None => break,
            Some(Ok(blk)) => {
                if let CompactingBlock::DataBlock {
                    field_id,
                    data_block,
                    ..
                } = blk
                {
                    let (column_id, _) = utils::split_id(field_id);
                    if !cid_col_name_map.contains_key(&column_id) {
                        continue;
                    }
                    *cid_fid_count_map.entry(column_id).or_default() += 1;

                    // Check if there is last hash value that not stored.
                    if let Some((time_range, last_fid)) = last_hashed_tr_fid {
                        if last_fid != field_id {
                            fid_tr_hash_val_map
                                .entry(last_fid)
                                .or_default()
                                .push((time_range, hasher.finalize().into()));
                            hasher.reset();
                        }
                    }
                    if let Some(blk_time_range) = data_block.time_range() {
                        // Get trunced time range by DataBlock.time[0]
                        // TODO: Data block may be split into multi time ranges.
                        let (min_ts, max_ts) = match calc_block_partial_time_range(
                            blk_time_range.0,
                            time_range,
                            time_range_nanosec,
                        ) {
                            Ok(tr) => tr,
                            Err(e) => return Err(e),
                        };

                        // Calculate and store the hash value of data in time range
                        let hash_vec = fid_tr_hash_val_map.entry(field_id).or_default();
                        if blk_time_range.1 > max_ts {
                            // Time range of data block need split.
                            let min_idx =
                                hash_partial_datablock(&mut hasher, &data_block, 0, max_ts);
                            hash_vec
                                .push((TimeRange::new(min_ts, max_ts), hasher.finalize().into()));
                            hasher.reset();
                            hash_partial_datablock(
                                &mut hasher,
                                &data_block,
                                min_idx,
                                Timestamp::MIN,
                            );
                            last_hashed_tr_fid =
                                Some((TimeRange::new(max_ts, blk_time_range.1), field_id));
                        } else {
                            hash_partial_datablock(&mut hasher, &data_block, 0, Timestamp::MIN);
                            last_hashed_tr_fid = Some((TimeRange::new(min_ts, max_ts), field_id));
                        }
                    } else {
                        // Ignore: Case argument decode_non_overlap_blocks in CompactIterator::new()
                        // is set to true, we may ignore it.
                    }
                };
            }
            Some(Err(e)) => {
                return Err(Error::CommonError {
                    reason: format!(
                        "error getting hashes for ts_family {} when compacting: {:?}",
                        ts_family_id, e
                    ),
                });
            }
        }
    }
    if let Some((tr, last_fid)) = last_hashed_tr_fid {
        fid_tr_hash_val_map
            .entry(last_fid)
            .or_default()
            .push((tr, hasher.finalize().into()));
    }

    Ok((fid_tr_hash_val_map, cid_fid_count_map))
}

#[cfg(test)]
mod test {
    use std::{
        collections::{BTreeMap, HashMap},
        default,
        rc::Rc,
        sync::Arc,
    };

    use blake3::Hasher;
    use chrono::{Duration, NaiveDateTime};
    use meta::meta_client::{MetaRef, RemoteMetaManager};
    use minivec::MiniVec;
    use models::{
        codec::Encoding,
        schema::{
            ColumnType, DatabaseOptions, DatabaseSchema, TableColumn, TableSchema, TenantOptions,
            TskvTableSchema,
        },
        Timestamp, ValueType,
    };
    use protos::{
        kv_service::WritePointsRpcRequest,
        models::{self as fb_models, FieldType},
        models_helper,
    };
    use tokio::{runtime, sync::mpsc};

    use crate::{
        compaction::{
            check::{
                get_default_time_range, hash_to_string, ColumnHashTreeNode, TimeRangeHashTreeNode,
            },
            FlushReq,
        },
        engine::Engine,
        summary::SummaryTask,
        tsm::{codec::DataBlockEncoding, DataBlock},
        version_set::VersionSet,
        Options, TimeRange, TsKv, TseriesFamilyId,
    };

    use super::{
        calc_block_partial_time_range, find_timestamp, hash_partial_datablock, Hash,
        TableHashTreeNode,
    };

    fn parse_nanos(datetime: &str) -> Timestamp {
        NaiveDateTime::parse_from_str(datetime, "%Y-%m-%d %H:%M:%S")
            .unwrap()
            .timestamp_nanos()
    }

    #[test]
    fn test_calc_blcok_time_range() {
        fn get_args(datetime: &str) -> (Timestamp, Duration, i64) {
            let datetime = NaiveDateTime::parse_from_str(datetime, "%Y-%m-%d %H:%M:%S").unwrap();
            let timestamp = datetime.timestamp_nanos();
            let duration = Duration::minutes(30);
            let duration_nanos = duration.num_nanoseconds().unwrap();
            (timestamp, duration, duration_nanos)
        }

        let (a, b, c) = get_args("2023-01-01 00:29:01");
        assert_eq!(
            (
                parse_nanos("2023-01-01 00:00:00"),
                parse_nanos("2023-01-01 00:30:00")
            ),
            calc_block_partial_time_range(a, b, c).unwrap(),
        );

        let (a, b, c) = get_args("2023-01-01 00:30:01");
        assert_eq!(
            (
                parse_nanos("2023-01-01 00:30:00"),
                parse_nanos("2023-01-01 01:00:00")
            ),
            calc_block_partial_time_range(a, b, c).unwrap(),
        );
    }

    #[test]
    fn test_find_timestamp() {
        let timestamps = vec![
            parse_nanos("2023-01-01 00:01:00"),
            parse_nanos("2023-01-01 00:02:00"),
            parse_nanos("2023-01-01 00:03:00"),
            parse_nanos("2023-01-01 00:04:00"),
            parse_nanos("2023-01-01 00:05:00"),
        ];

        assert_eq!(
            0,
            find_timestamp(&timestamps, parse_nanos("2023-01-01 00:00:00")),
        );
        assert_eq!(
            3,
            find_timestamp(&timestamps, parse_nanos("2023-01-01 00:03:30"))
        );
        assert_eq!(
            3,
            find_timestamp(&timestamps, parse_nanos("2023-01-01 00:04:00"))
        );
        assert_eq!(
            5,
            find_timestamp(&timestamps, parse_nanos("2023-01-01 00:30:00"))
        );
        assert_eq!(5, find_timestamp(&timestamps, Timestamp::MIN),);
    }

    fn data_block_partial_to_bytes(data_block: &DataBlock, from: usize, to: usize) -> Vec<u8> {
        let mut ret: Vec<u8> = vec![];
        for i in from..to {
            let v = data_block
                .get(i)
                .unwrap_or_else(|| panic!("data block has at least {} items", i));
            ret.append(v.to_bytes().to_vec().as_mut());
        }
        ret
    }

    #[test]
    fn test_hash_partial_datablock() {
        let timestamps = vec![
            parse_nanos("2023-01-01 00:01:00"),
            parse_nanos("2023-01-01 00:02:00"),
            parse_nanos("2023-01-01 00:03:00"),
            parse_nanos("2023-01-01 00:04:00"),
            parse_nanos("2023-01-01 00:05:00"),
            parse_nanos("2023-01-01 00:06:00"),
        ];
        #[rustfmt::skip]
        let data_blocks = vec![
            DataBlock::U64 { ts: timestamps.clone(), val: vec![1, 2, 3, 4, 5, 6], enc: DataBlockEncoding::default() },
            DataBlock::I64 { ts: timestamps.clone(), val: vec![1, 2, 3, 4, 5, 6], enc: DataBlockEncoding::default() },
            DataBlock::Str { ts: timestamps.clone(),
                val: vec![
                    MiniVec::from("1"), MiniVec::from("2"), MiniVec::from("3"), MiniVec::from("4"), MiniVec::from("5"), MiniVec::from("6")
                ], enc: DataBlockEncoding::default()
            },
            DataBlock::F64 { ts: timestamps.clone(), val: vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0], enc: DataBlockEncoding::default() },
            DataBlock::Bool { ts: timestamps, val: vec![true, false, true, false, true, false], enc: DataBlockEncoding::default() },
        ];

        for data_block in data_blocks {
            let mut hasher_blk = Hasher::new();
            let min_idx = hash_partial_datablock(
                &mut hasher_blk,
                &data_block,
                0,
                parse_nanos("2023-01-01 00:04:00"),
            );
            assert_eq!(3, min_idx);
            let mut hasher_cmp = Hasher::new();
            assert_eq!(
                hasher_cmp
                    .update(data_block_partial_to_bytes(&data_block, 0, 3).as_slice())
                    .finalize(),
                hasher_blk.finalize()
            );

            let mut hasher_blk = Hasher::new();
            let min_idx =
                hash_partial_datablock(&mut hasher_blk, &data_block, min_idx, Timestamp::MIN);
            assert_eq!(6, min_idx);
            let mut hasher_cmp = Hasher::new();
            assert_eq!(
                hasher_cmp
                    .update(data_block_partial_to_bytes(&data_block, 3, 6).as_slice())
                    .finalize(),
                hasher_blk.finalize()
            );
        }
    }

    const TIME_COL_NAME: &str = "time";
    const U64_COL_NAME: &str = "col_u64";
    const I64_COL_NAME: &str = "col_i64";
    const F64_COL_NAME: &str = "col_f64";
    const STR_COL_NAME: &str = "col_str";
    const BOOL_COL_NAME: &str = "col_bool";

    fn create_write_batch_args(
        data_blocks: &[DataBlock],
        timestamps: &mut Vec<Timestamp>,
        fields: &mut Vec<Vec<(&str, FieldType, Vec<u8>)>>,
    ) {
        let mut u64_vec = vec![];
        let mut i64_vec = vec![];
        let mut f64_vec = vec![];
        let mut str_vec = vec![];
        let mut bool_vec = vec![];
        for data_block in data_blocks {
            match data_block {
                DataBlock::U64 { ts, val, .. } => {
                    for (t, v) in ts.iter().zip(val) {
                        u64_vec.push((*t, v.to_be_bytes().to_vec()));
                    }
                }
                DataBlock::I64 { ts, val, .. } => {
                    for (t, v) in ts.iter().zip(val) {
                        i64_vec.push((*t, v.to_be_bytes().to_vec()));
                    }
                }
                DataBlock::F64 { ts, val, .. } => {
                    for (t, v) in ts.iter().zip(val) {
                        f64_vec.push((*t, v.to_be_bytes().to_vec()));
                    }
                }
                DataBlock::Str { ts, val, .. } => {
                    for (t, v) in ts.iter().zip(val) {
                        str_vec.push((*t, v.to_vec()));
                    }
                }
                DataBlock::Bool { ts, val, .. } => {
                    for (t, v) in ts.iter().zip(val) {
                        bool_vec.push((*t, if *v { vec![1_u8] } else { vec![0_u8] }));
                    }
                }
            }
        }

        u64_vec.sort_by_key(|v| v.0);
        i64_vec.sort_by_key(|v| v.0);
        f64_vec.sort_by_key(|v| v.0);
        str_vec.sort_by_key(|v| v.0);
        bool_vec.sort_by_key(|v| v.0);

        #[allow(clippy::type_complexity)]
        fn write_vec_into_map(
            vec: Vec<(i64, Vec<u8>)>,
            col_name: &'static str,
            field_type: FieldType,
            map: &mut BTreeMap<Timestamp, Vec<(&str, FieldType, Vec<u8>)>>,
        ) {
            vec.into_iter().for_each(|(t, v)| {
                let entry = map.entry(t).or_default();
                entry.push((col_name, field_type, v));
            });
        }
        #[allow(clippy::type_complexity)]
        let mut map: BTreeMap<Timestamp, Vec<(&str, FieldType, Vec<u8>)>> = BTreeMap::new();
        write_vec_into_map(u64_vec, U64_COL_NAME, FieldType::Unsigned, &mut map);
        write_vec_into_map(i64_vec, I64_COL_NAME, FieldType::Integer, &mut map);
        write_vec_into_map(f64_vec, F64_COL_NAME, FieldType::Float, &mut map);
        write_vec_into_map(str_vec, STR_COL_NAME, FieldType::String, &mut map);
        write_vec_into_map(bool_vec, BOOL_COL_NAME, FieldType::Boolean, &mut map);

        map.into_iter().for_each(|(t, v)| {
            timestamps.push(t);
            fields.push(v);
        });
    }

    fn create_write_batch(
        timestamps: Vec<i64>,
        database: &str,
        table: &str,
        rows: Vec<Vec<(&str, FieldType, Vec<u8>)>>,
    ) -> WritePointsRpcRequest {
        let mut rows_ref = Vec::with_capacity(rows.len());
        for cols in rows.iter() {
            let mut cols_ref = Vec::with_capacity(cols.len());
            for (col, ft, v) in cols.iter() {
                cols_ref.push((*col, *ft, v.as_slice()));
            }
            rows_ref.push(cols_ref);
        }

        let mut points = vec![];
        let mut fbb = flatbuffers::FlatBufferBuilder::new();
        for (timestamp, v) in timestamps.into_iter().zip(rows_ref.into_iter()) {
            let db = fbb.create_vector(database.as_bytes());
            let table = fbb.create_vector(table.as_bytes());
            let tags = models_helper::create_tags(&mut fbb, vec![("ta", "a1"), ("tb", "b1")]);
            let fields = models_helper::create_fields(&mut fbb, v);
            let point = models_helper::create_point(&mut fbb, timestamp, db, table, tags, fields);
            points.push(point);
        }
        let point_vec = fbb.create_vector(&points);
        let db = fbb.create_vector(database.as_bytes());
        let points = fb_models::Points::create(
            &mut fbb,
            &fb_models::PointsArgs {
                db: Some(db),
                points: Some(point_vec),
            },
        );
        fbb.finish(points, None);
        let points = fbb.finished_data().to_vec();
        WritePointsRpcRequest { version: 1, points }
    }

    fn data_block_to_hash_tree(
        data_block: &DataBlock,
        time_range_nanosec: i64,
    ) -> Vec<(TimeRange, Hash)> {
        let time_range = Duration::nanoseconds(time_range_nanosec);
        let mut tr_hashes: Vec<(TimeRange, Hash)> = Vec::new();
        let mut hasher = Hasher::new();

        if let Some(blk_time_range) = data_block.time_range() {
            // Get trunced time range by DataBlock.time[0]
            let (min_ts, max_ts) =
                calc_block_partial_time_range(blk_time_range.0, time_range, time_range_nanosec)
                    .unwrap();

            // Calculate and store the hash value of data in time range
            if blk_time_range.1 > max_ts {
                // Time range of data block need to split.
                let min_idx = hash_partial_datablock(&mut hasher, data_block, 0, max_ts);
                tr_hashes.push((TimeRange::new(min_ts, max_ts), hasher.finalize().into()));
                hasher.reset();
                hash_partial_datablock(&mut hasher, data_block, min_idx, Timestamp::MIN);
                tr_hashes.push((
                    TimeRange::new(max_ts, blk_time_range.1),
                    hasher.finalize().into(),
                ));
            } else {
                hash_partial_datablock(&mut hasher, data_block, 0, Timestamp::MIN);
                tr_hashes.push((TimeRange::new(min_ts, max_ts), hasher.finalize().into()));
            }
        }

        tr_hashes
            .into_iter()
            .map(|(tr, hash)| {
                let mut hasher = Hasher::new();
                hasher.update(&hash);
                (tr, hasher.finalize().into())
            })
            .collect()
    }

    fn check_hash_tree_node(
        col_values: &[TimeRangeHashTreeNode],
        cmp_values: &[(TimeRange, Hash)],
        col_name: &str,
    ) {
        assert_eq!(col_values.len(), cmp_values.len());
        for (a, b) in col_values.iter().zip(cmp_values.iter()) {
            assert_eq!(a.min_ts, b.0.min_ts, "Col '{}' min_ts compare", col_name);
            assert_eq!(a.max_ts, b.0.max_ts, "Col '{}' max_ts compare", col_name);
            assert_eq!(a.hash, b.1, "Col '{}' hash compare", col_name);
        }
    }

    #[test]
    fn test_get_ts_family_hash_tree() {
        let config_file_path = "../config/config_31001.toml";
        let base_dir = "/tmp/test/repair/1".to_string();
        let wal_dir = "/tmp/test/repair/1/wal".to_string();
        let log_dir = "/tmp/test/repair/1/log".to_string();
        trace::init_default_global_tracing(&log_dir, "test.log", "debug");
        let _ = std::fs::remove_dir(&base_dir);
        let tenant_name = "cnosdb".to_string();
        let database_name = "test_get_ts_family_hash_tree".to_string();
        let ts_family_id: TseriesFamilyId = 1;
        let table_name = "test_table".to_string();
        let sec_1 = Duration::seconds(1).to_std().unwrap();

        let timestamps = vec![
            parse_nanos("2023-01-01 00:01:00"),
            parse_nanos("2023-01-01 00:02:00"),
            parse_nanos("2023-01-01 00:03:00"),
            parse_nanos("2023-02-01 00:01:00"),
            parse_nanos("2023-02-01 00:02:00"),
            parse_nanos("2023-02-01 00:03:00"),
        ];
        #[rustfmt::skip]
        let data_blocks = vec![
            DataBlock::U64 { ts: timestamps.clone(), val: vec![1, 2, 3, 4, 5, 6], enc: DataBlockEncoding::default() },
            DataBlock::I64 { ts: timestamps.clone(), val: vec![1, 2, 3, 4, 5, 6], enc: DataBlockEncoding::default() },
            DataBlock::F64 { ts: timestamps.clone(), val: vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0], enc: DataBlockEncoding::default() },
            DataBlock::Str { ts: timestamps.clone(),
                val: vec![
                    MiniVec::from("1"), MiniVec::from("2"), MiniVec::from("3"), MiniVec::from("4"), MiniVec::from("5"), MiniVec::from("6")
                ], enc: DataBlockEncoding::default()
            },
            DataBlock::Bool { ts: timestamps, val: vec![true, false, true, false, true, false], enc: DataBlockEncoding::default() },
        ];
        #[rustfmt::skip]
        let columns = vec![
            TableColumn::new(0, TIME_COL_NAME.to_string(), ColumnType::Time, Default::default()),
            TableColumn::new(1, U64_COL_NAME.to_string(), ColumnType::Field(ValueType::Unsigned), Default::default()),
            TableColumn::new(2, I64_COL_NAME.to_string(), ColumnType::Field(ValueType::Integer), Default::default()),
            TableColumn::new(3, F64_COL_NAME.to_string(), ColumnType::Field(ValueType::Float), Default::default()),
            TableColumn::new(4, STR_COL_NAME.to_string(), ColumnType::Field(ValueType::String), Default::default()),
            TableColumn::new(5, BOOL_COL_NAME.to_string(), ColumnType::Field(ValueType::Boolean), Default::default()),
        ];

        let rt = Arc::new(
            runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap(),
        );

        let mut config = config::get_config(config_file_path);
        config.storage.path = base_dir;
        config.wal.path = wal_dir;
        config.wal.sync = true;
        config.log.path = log_dir;
        let opt = Options::from(&config);
        let meta: MetaRef = Arc::new(RemoteMetaManager::new(config.cluster.clone()));
        let _ = meta
            .tenant_manager()
            .create_tenant(tenant_name.clone(), TenantOptions::default());
        let meta_client = meta.tenant_manager().tenant_meta(&tenant_name).unwrap();
        let engine = rt
            .block_on(TsKv::open(config.cluster, opt, rt.clone()))
            .unwrap();

        // Create database and ts_family
        {
            let mut database_schema = DatabaseSchema::new(&tenant_name, &database_name);
            database_schema
                .config
                .with_ttl(DatabaseOptions::DEFAULT_TTL);
            if let Err(e) = meta_client.drop_db(&database_name) {
                println!(
                    "Repair: failed to drop database '{}': {:?}",
                    &database_name, e
                );
            }
            meta_client.create_db(database_schema.clone()).unwrap();
            meta_client
                .create_table(&TableSchema::TsKvTableSchema(TskvTableSchema::new(
                    tenant_name.clone(),
                    database_name.clone(),
                    table_name.clone(),
                    columns,
                )))
                .unwrap();

            let _ = engine.drop_database(&tenant_name, &database_name);
            let database = rt
                .block_on(engine.create_database(&database_schema))
                .unwrap();
            let ts_family = rt.block_on(database.write()).add_tsfamily(
                ts_family_id,
                1,
                engine.summary_task_sender(),
                engine.flush_task_sender(),
            );
            assert_eq!(1, rt.block_on(ts_family.read()).tf_id());
        }

        // Get created database and ts_family
        let database_ref = rt
            .block_on(engine.get_db(&tenant_name, &database_name))
            .unwrap_or_else(|e| panic!("created database '{}' exists: {:?}", &database_name, e));
        let schemas = rt.block_on(database_ref.read()).get_schemas();
        let time_range_nanosec = get_default_time_range(schemas).unwrap();
        let ts_family_ref = rt
            .block_on(database_ref.read())
            .get_tsfamily(ts_family_id)
            .unwrap_or_else(|| {
                panic!("created ts_family '{}' exists", ts_family_id);
            });

        // Write data to database and ts_family
        {
            let mut timestamps: Vec<Timestamp> = Vec::new();
            let mut fields: Vec<Vec<(&str, FieldType, Vec<u8>)>> = Vec::new();
            create_write_batch_args(&data_blocks, &mut timestamps, &mut fields);
            let write_batch = create_write_batch(timestamps, &database_name, &table_name, fields);
            let points = flatbuffers::root::<fb_models::Points>(&write_batch.points).unwrap();
            models_helper::print_points(points);
            rt.block_on(engine.write(ts_family_id, &tenant_name, write_batch))
                .unwrap();

            let mut ts_family_wlock = rt.block_on(ts_family_ref.write());
            ts_family_wlock.switch_to_immutable();
            let immut_cache_ref = ts_family_wlock
                .super_version()
                .caches
                .immut_cache
                .first()
                .expect("translated immutable memory cache exist")
                .clone();
            ts_family_wlock.wrap_flush_req(true);
            drop(ts_family_wlock);

            let mut check_num = 0;
            loop {
                rt.block_on(async {
                    tokio::time::sleep(sec_1).await;
                });
                if immut_cache_ref.read().flushed {
                    drop(immut_cache_ref);
                    break;
                }
                check_num += 1;
                if check_num >= 10 {
                    println!("Repair: warn: flushing takes more than 10 seconds.");
                }
            }
        }

        // Get hash values and check them.
        {
            let trees = rt
                .block_on(async {
                    database_ref
                        .read()
                        .await
                        .get_ts_family_hash_tree(ts_family_id)
                        .await
                })
                .unwrap();
            assert_eq!(trees.len(), 1);
            assert_eq!(trees[0].table, table_name);
            assert_eq!(trees[0].columns.len(), 5);
            for col in trees[0].columns.iter() {
                match col.column.as_str() {
                    U64_COL_NAME => {
                        let col_u64_hashes =
                            data_block_to_hash_tree(&data_blocks[0], time_range_nanosec);
                        check_hash_tree_node(&col.values, &col_u64_hashes, col.column.as_str());
                    }
                    I64_COL_NAME => {
                        let col_i64_hashes =
                            data_block_to_hash_tree(&data_blocks[1], time_range_nanosec);
                        check_hash_tree_node(&col.values, &col_i64_hashes, col.column.as_str());
                    }
                    F64_COL_NAME => {
                        let col_f64_hashes =
                            data_block_to_hash_tree(&data_blocks[2], time_range_nanosec);
                        check_hash_tree_node(&col.values, &col_f64_hashes, col.column.as_str());
                    }
                    STR_COL_NAME => {
                        let col_str_hashes =
                            data_block_to_hash_tree(&data_blocks[3], time_range_nanosec);
                        check_hash_tree_node(&col.values, &col_str_hashes, col.column.as_str());
                    }
                    BOOL_COL_NAME => {
                        let col_bool_hashes =
                            data_block_to_hash_tree(&data_blocks[4], time_range_nanosec);
                        check_hash_tree_node(&col.values, &col_bool_hashes, col.column.as_str());
                    }
                    _ => {}
                }
            }
        }

        rt.block_on(engine.close());
    }
}