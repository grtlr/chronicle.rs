// Copyright 2021 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::responses::*;
use ::rocket::{
    fairing::{
        Fairing,
        Info,
        Kind,
    },
    get,
    http::ContentType,
    response::{
        content,
        Responder,
    },
    serde::json::Json,
    Build,
    Data,
    Request,
    Response,
    State,
};
use anyhow::anyhow;
use bee_message::{
    milestone::Milestone,
    payload::Payload,
    prelude::{
        Ed25519Address,
        Message,
        MessageId,
        MilestoneIndex,
        OutputId,
        TransactionId,
    },
};
use chronicle_common::{
    config::PartitionConfig,
    metrics::{
        prometheus::{
            self,
            Encoder,
            TextEncoder,
        },
        INCOMING_REQUESTS,
        REGISTRY,
        RESPONSE_CODE_COLLECTOR,
        RESPONSE_TIME_COLLECTOR,
    },
    SyncRange,
};
use chronicle_storage::{
    access::{
        MessageMetadata,
        OutputRes,
        PartitionId,
        Partitioned,
    },
    keyspaces::ChronicleKeyspace,
};
use futures::{
    StreamExt,
    TryStreamExt,
};
use hex::FromHex;
use std::{
    borrow::Borrow,
    collections::{
        HashMap,
        HashSet,
        VecDeque,
    },
    convert::{
        TryFrom,
        TryInto,
    },
    fmt::Debug,
    io::Cursor,
    path::PathBuf,
    str::FromStr,
    time::SystemTime,
};

#[allow(missing_docs)]
pub fn construct_rocket() -> Rocket<Build> {
    ::rocket::build()
        .mount(
            "/api",
            routes![
                options,
                info,
                metrics,
                service,
                sync,
                get_message,
                get_message_metadata,
                get_message_children,
                get_message_by_index,
                get_output_by_transaction_id,
                get_output,
                get_ed25519_outputs,
                get_transactions_for_address,
                get_transaction_for_message,
                get_transaction_included_message,
                get_milestone,
                get_analytics
            ],
        )
        .attach(CORS)
        .attach(RequestTimer)
        .register("/", catchers![internal_error, not_found])
}

struct CORS;

#[::rocket::async_trait]
impl Fairing for CORS {
    fn info(&self) -> ::rocket::fairing::Info {
        Info {
            name: "Add CORS Headers",
            kind: Kind::Response,
        }
    }

    async fn on_response<'r>(&self, _request: &'r Request<'_>, response: &mut Response<'r>) {
        response.set_raw_header("Access-Control-Allow-Origin", "*");
        response.set_raw_header("Access-Control-Allow-Methods", "GET, OPTIONS");
        response.set_raw_header("Access-Control-Allow-Headers", "*");
        response.set_raw_header("Access-Control-Allow-Credentials", "true");
    }
}

pub struct RequestTimer;

#[derive(Copy, Clone)]
struct TimerStart(Option<SystemTime>);

#[::rocket::async_trait]
impl Fairing for RequestTimer {
    fn info(&self) -> Info {
        Info {
            name: "Request Timer",
            kind: Kind::Request | Kind::Response,
        }
    }

    /// Stores the start time of the request in request-local state.
    async fn on_request(&self, request: &mut Request<'_>, _: &mut Data<'_>) {
        // Store a `TimerStart` instead of directly storing a `SystemTime`
        // to ensure that this usage doesn't conflict with anything else
        // that might store a `SystemTime` in request-local cache.
        request.local_cache(|| TimerStart(Some(SystemTime::now())));
        INCOMING_REQUESTS.inc();
    }

    /// Adds a header to the response indicating how long the server took to
    /// process the request.
    async fn on_response<'r>(&self, req: &'r Request<'_>, res: &mut Response<'r>) {
        let start_time = req.local_cache(|| TimerStart(None));
        if let Some(Ok(duration)) = start_time.0.map(|st| st.elapsed()) {
            let ms = (duration.as_secs() * 1000 + duration.subsec_millis() as u64) as f64;
            RESPONSE_TIME_COLLECTOR
                .with_label_values(&[&format!("{} {}", req.method(), req.uri())])
                .observe(ms)
        }
        match res.status().code {
            500..=599 => RESPONSE_CODE_COLLECTOR
                .with_label_values(&[&res.status().code.to_string(), "500"])
                .inc(),
            400..=499 => RESPONSE_CODE_COLLECTOR
                .with_label_values(&[&res.status().code.to_string(), "400"])
                .inc(),
            300..=399 => RESPONSE_CODE_COLLECTOR
                .with_label_values(&[&res.status().code.to_string(), "300"])
                .inc(),
            200..=299 => RESPONSE_CODE_COLLECTOR
                .with_label_values(&[&res.status().code.to_string(), "200"])
                .inc(),
            100..=199 => RESPONSE_CODE_COLLECTOR
                .with_label_values(&[&res.status().code.to_string(), "100"])
                .inc(),
            _ => (),
        }
    }
}

impl<'r> Responder<'r, 'static> for ListenerError {
    fn respond_to(self, _req: &'r Request<'_>) -> ::rocket::response::Result<'static> {
        let err = ErrorBody::from(self);
        let string = serde_json::to_string(&err).map_err(|e| {
            error!("JSON failed to serialize: {:?}", e);
            Status::InternalServerError
        })?;

        Response::build()
            .sized_body(None, Cursor::new(string))
            .status(err.status)
            .header(ContentType::JSON)
            .ok()
    }
}

impl<'r> Responder<'r, 'static> for ListenerResponse {
    fn respond_to(self, req: &'r Request<'_>) -> ::rocket::response::Result<'static> {
        let success = SuccessBody::from(self);
        let string = serde_json::to_string(&success).map_err(|e| {
            error!("JSON failed to serialize: {:?}", e);
            Status::InternalServerError
        })?;

        content::Json(string).respond_to(req)
    }
}

type ListenerResult = Result<ListenerResponse, ListenerError>;

#[options("/<_path..>")]
async fn options(_path: PathBuf) {}

#[get("/<keyspace>/info")]
async fn info(keyspaces: &State<HashSet<String>>, keyspace: String) -> ListenerResult {
    if !keyspaces.contains(&keyspace) {
        return Err(ListenerError::InvalidKeyspace(keyspace));
    }
    let version = std::env!("CARGO_PKG_VERSION").to_string();
    let service = Scope::lookup::<Service>(0)
        .await
        .ok_or_else(|| ListenerError::NotFound)?;
    let is_healthy = !std::iter::once(&service)
        .chain(service.microservices.values())
        .any(|service| !service.is_running());
    Ok(ListenerResponse::Info {
        name: "Chronicle (keyspace)".into(),
        version,
        is_healthy,
    })
}

#[get("/metrics")]
async fn metrics() -> Result<String, ListenerError> {
    let encoder = TextEncoder::new();
    let mut buffer = Vec::new();
    encoder
        .encode(&REGISTRY.gather(), &mut buffer)
        .map_err(|e| ListenerError::Other(e.into()))?;

    let res_custom = String::from_utf8(std::mem::take(&mut buffer)).map_err(|e| ListenerError::Other(e.into()))?;

    encoder
        .encode(&prometheus::gather(), &mut buffer)
        .map_err(|e| ListenerError::Other(e.into()))?;

    let res_default = String::from_utf8(buffer).map_err(|e| ListenerError::Other(e.into()))?;

    Ok(format!("{}{}", res_custom, res_default))
}

#[get("/service")]
async fn service() -> Result<Json<Service>, ListenerError> {
    let service = Scope::lookup::<Service>(0)
        .await
        .ok_or_else(|| ListenerError::NotFound)?;
    Ok(Json(service))
}

#[get("/<keyspace>/sync")]
async fn sync(keyspaces: &State<HashSet<String>>, keyspace: String) -> Result<Json<SyncData>, ListenerError> {
    if !keyspaces.contains(&keyspace) {
        return Err(ListenerError::InvalidKeyspace(keyspace));
    }
    let keyspace = ChronicleKeyspace::new(keyspace);
    SyncData::try_fetch(&keyspace, &SyncRange::default(), 3)
        .await
        .map(|s| Json(s))
        .map_err(|e| ListenerError::Other(e.into()))
}

async fn query<O, K, V, S>(
    keyspace: S,
    key: K,
    variables: V,
    page_size: Option<i32>,
    paging_state: Option<Vec<u8>>,
) -> Result<O, ListenerError>
where
    S: 'static + Select<K, V, O>,
    K: 'static + Send + Sync + Clone + TokenEncoder,
    V: 'static + Send + Sync + Clone,
    O: 'static + Send + Sync + Clone + Debug + RowsDecoder,
{
    let request = keyspace.select::<O>(&key, &variables).consistency(Consistency::One);
    if let Some(page_size) = page_size {
        request.page_size(page_size).paging_state(&paging_state)
    } else {
        request.paging_state(&paging_state)
    }
    .build()?
    .worker()
    .with_retries(3)
    .get_local()
    .await
    .map_err(|e| e.into())
    .and_then(|res| res.ok_or_else(|| ListenerError::NoResults))
}
async fn page<K, O>(
    keyspace: String,
    hint: Hint,
    page_size: usize,
    state: &mut Option<StateData>,
    partition_config: &PartitionConfig,
    key: K,
) -> Result<Vec<Partitioned<O>>, ListenerError>
where
    K: 'static + Send + Sync + Clone + TokenEncoder,
    O: 'static + Send + Sync + Clone + Debug + HasMilestoneIndex,
    ChronicleKeyspace: Select<(K, PartitionId), Bee<MilestoneIndex>, Paged<VecDeque<Partitioned<O>>>>,
    Paged<VecDeque<Partitioned<O>>>: RowsDecoder,
{
    let total_start_time = std::time::Instant::now();
    let mut start_time = total_start_time;
    // The milestone chunk, i.e. how many sequential milestones go on a partition at a time
    let milestone_chunk = partition_config.milestone_chunk_size as usize;

    let keyspace = ChronicleKeyspace::new(keyspace);
    // Get the list of partitions which contain records for this request.
    // These may have been passed in by the client, in which case we do not need
    // to query for them.
    let (latest_milestone, partition_ids) = match state {
        Some(state) => {
            if state.partition_ids.is_empty() {
                return Err(ListenerError::InvalidState);
            }
            let latest_milestone = state
                .last_milestone_index
                .unwrap_or_else(|| state.partition_ids.first().map(|(i, _)| i.0).unwrap());
            (latest_milestone, state.partition_ids.clone())
        }
        None => {
            let partition_ids = query::<Iter<(Bee<MilestoneIndex>, PartitionId)>, _, _, _>(
                keyspace.clone(),
                hint.hint,
                hint.variant,
                None,
                None,
            )
            .await?;
            if partition_ids.is_empty() {
                return Err(ListenerError::NoResults);
            }
            let mut partition_ids = partition_ids.map(|(ms, p)| (ms.into_inner(), p)).collect::<Vec<_>>();
            let (first_partition_id, latest_milestone) = partition_ids
                .iter()
                .max_by_key(|(index, _)| index)
                .map(|(index, id)| (*id, index.0))
                .unwrap();
            // Reorder the partitions list so we start with the correct partition id
            let i = partition_ids
                .iter()
                .position(|&(_, partition_id)| first_partition_id == partition_id);
            if let Some(i) = i {
                partition_ids = partition_ids[i..]
                    .iter()
                    .chain(partition_ids[..i].iter())
                    .cloned()
                    .collect();
            }
            *state = Some((None, None, None, partition_ids.clone()).into());
            (latest_milestone, partition_ids)
        }
    };

    // This is safe because we set the value above
    let mut state = state.as_mut().unwrap();

    // The last partition id that we got results from. This is sent back and forth between
    // the requestor to keep track of pages.
    let prev_last_partition_id = state.last_partition_id.take();
    // The last milestone index we got results from.
    let prev_last_milestone_index = state.last_milestone_index.take();
    let prev_paging_state = state.paging_state.take();

    debug!(
        "Setup time: {} ms",
        (std::time::Instant::now() - start_time).as_millis()
    );
    start_time = std::time::Instant::now();

    debug!(
        "Reorder time: {} ms",
        (std::time::Instant::now() - start_time).as_millis()
    );

    // This will hold lists of results keyed by partition id
    let mut list_map = HashMap::new();

    // The number of queries we will dispatch at a time.
    // Two queries seems to cover most cases. In extreme circumstances we can fetch more as needed.
    let fetch_size = 2;
    // The resulting list
    let mut results = Vec::new();
    let mut depleted_partitions = HashSet::new();
    let mut last_index_map = HashMap::new();
    last_index_map.insert(
        partition_ids[0].1,
        prev_last_milestone_index.unwrap_or(latest_milestone),
    );
    let mut loop_timings = HashMap::new();
    for (partition_ind, (index, partition_id)) in partition_ids.iter().enumerate().cycle() {
        if !last_index_map.contains_key(partition_id) {
            last_index_map.insert(*partition_id, index.0);
        }
        debug!("Gathering results from partition {}", partition_id);
        // Make sure we stop iterating if all of our partitions are depleted.
        if depleted_partitions.len() == partition_ids.len() {
            break;
        }
        // Skip depleted partitions
        if depleted_partitions.contains(partition_id) {
            debug!("Skipping partition");
            continue;
        }

        // Fetch a chunk of results if we need them to fill the page size
        if !list_map.contains_key(partition_id) {
            start_time = std::time::Instant::now();
            let fetch_ids =
                (partition_ind..partition_ind + fetch_size).filter_map(|ind| partition_ids.get(ind).map(|v| v.1));
            let res = futures::stream::iter(fetch_ids.clone())
                .map(|i| (key.clone(), keyspace.clone(), prev_paging_state.clone(), i))
                .then(|(key, keyspace, prev_paging_state, partition_id)| async move {
                    debug!(
                        "Fetching results for partition id: {}, milestone: {}, with paging state: {:?}",
                        partition_id,
                        latest_milestone,
                        prev_last_partition_id.map(|id| partition_id == id)
                    );
                    query::<Paged<VecDeque<Partitioned<O>>>, _, _, _>(
                        keyspace,
                        (key, partition_id),
                        Bee(MilestoneIndex(latest_milestone)),
                        Some(page_size as i32),
                        prev_last_partition_id.and_then(|id| if partition_id == id { prev_paging_state } else { None }),
                    )
                    .await
                })
                .try_collect::<Vec<Paged<VecDeque<Partitioned<O>>>>>()
                .await?;
            debug!(
                "Fetch time: {} ms",
                (std::time::Instant::now() - start_time).as_millis()
            );
            for (partition_id, list) in fetch_ids.zip(res) {
                list_map.insert(partition_id, list);
            }
        }
        let list = list_map
            .get_mut(&partition_id)
            .ok_or_else(|| anyhow!("Unexpected error retrieving list by partition!"))?;

        // Iterate the list, pulling records from the front until we hit
        // a milestone in the next chunk or run out
        loop {
            let loop_start_time = std::time::Instant::now();
            if !list.is_empty() {
                // If we're still looking at the same chunk
                if list[0].milestone_index() / milestone_chunk as u32
                    == last_index_map[partition_id] / milestone_chunk as u32
                {
                    // And we exceeded the page size
                    if results.len() >= page_size {
                        // Add more anyway if the milestone index is the same,
                        // because we won't be able to recover lost records
                        // with a paging state
                        if last_index_map[partition_id] == list[0].milestone_index() {
                            // debug!("Adding extra records past page_size");
                            results.push(list.pop_front().unwrap());
                            *loop_timings.entry("Adding additional").or_insert(0) +=
                                (std::time::Instant::now() - loop_start_time).as_nanos();
                        // Otherwise we can stop here and set our cookies
                        } else {
                            debug!("Finished a milestone");
                            state.last_partition_id = Some(*partition_id);
                            state.last_milestone_index = Some(list[0].milestone_index());
                            *loop_timings.entry("Finish Adding Additional").or_insert(0) +=
                                (std::time::Instant::now() - loop_start_time).as_nanos();
                            debug!(
                                "{:#?}",
                                loop_timings
                                    .iter()
                                    .map(|(k, v)| (k, format!("{} ms", *v as f32 / 1000000.0)))
                                    .collect::<HashMap<_, _>>()
                            );
                            debug!(
                                "Total time: {} ms",
                                (std::time::Instant::now() - total_start_time).as_millis()
                            );
                            return Ok(results);
                        }
                    // Otherwise, business as usual
                    } else {
                        let partitioned_value = list.pop_front().unwrap();
                        // debug!("Adding result normally");
                        last_index_map.insert(*partition_id, partitioned_value.milestone_index());
                        results.push(partitioned_value);
                        *loop_timings.entry("Adding normally").or_insert(0) +=
                            (std::time::Instant::now() - loop_start_time).as_nanos();
                    }
                // We hit a new chunk, so we want to look at the next partition now
                } else {
                    debug!("Hit a chunk boundary");
                    last_index_map.insert(*partition_id, list[0].milestone_index());
                    *loop_timings.entry("Chunk Boundary").or_insert(0) +=
                        (std::time::Instant::now() - loop_start_time).as_nanos();
                    break;
                }
            // The list is empty, but that doesn't necessarily mean there aren't more valid records on this partition.
            // So we will get the next page_size records by re-running the same query with the paging state
            // or just give it to the client if we already have enough records.
            } else {
                debug!("Results list is empty");
                if results.len() >= page_size {
                    debug!("...but we already have enough results so returning the paging state");
                    state.paging_state = list.paging_state.take();
                    state.last_partition_id = Some(*partition_id);
                    state.last_milestone_index = Some(latest_milestone);
                    *loop_timings.entry("Returning page_state").or_insert(0) +=
                        (std::time::Instant::now() - loop_start_time).as_nanos();
                    debug!(
                        "{:#?}",
                        loop_timings
                            .iter()
                            .map(|(k, v)| (k, format!("{} ms", *v as f32 / 1000000.0)))
                            .collect::<HashMap<_, _>>()
                    );
                    debug!(
                        "Total time: {} ms",
                        (std::time::Instant::now() - total_start_time).as_millis()
                    );
                    return Ok(results);
                } else {
                    debug!("...and we need more results");
                    if list.paging_state.is_some() {
                        debug!("......so we're querying for them");
                        *list = query::<Paged<VecDeque<Partitioned<O>>>, _, _, _>(
                            keyspace.clone(),
                            (key.clone(), *partition_id),
                            Bee(MilestoneIndex(latest_milestone)),
                            Some((page_size - results.len()) as i32),
                            list.paging_state.clone(),
                        )
                        .await?;
                        *loop_timings.entry("Requery").or_insert(0) +=
                            (std::time::Instant::now() - loop_start_time).as_nanos();
                    // Unless it didn't have one, in which case we mark it as a depleted partition and
                    // move on to the next one.
                    } else {
                        debug!("......but there's no paging state");
                        depleted_partitions.insert(*partition_id);
                        *loop_timings.entry("Depleted partition").or_insert(0) +=
                            (std::time::Instant::now() - loop_start_time).as_nanos();
                        break;
                    }
                }
            }
        }
    }

    debug!(
        "{:#?}",
        loop_timings
            .iter()
            .map(|(k, v)| (k, format!("{} ms", *v as f32 / 1000000.0)))
            .collect::<HashMap<_, _>>()
    );

    debug!(
        "Total time: {} ms",
        (std::time::Instant::now() - total_start_time).as_millis()
    );

    Ok(results)
}

#[get("/<keyspace>/messages/<message_id>")]
async fn get_message(keyspace: String, message_id: String, keyspaces: &State<HashSet<String>>) -> ListenerResult {
    if !keyspaces.contains(&keyspace) {
        return Err(ListenerError::InvalidKeyspace(keyspace));
    }
    let keyspace = ChronicleKeyspace::new(keyspace);
    let message_id = Bee(MessageId::from_str(&message_id).map_err(|e| ListenerError::BadParse(e.into()))?);
    query::<Bee<Message>, _, _, _>(keyspace, message_id, (), None, None)
        .await
        .and_then(|message| {
            message
                .into_inner()
                .try_into()
                .map_err(|e: Cow<'static, str>| anyhow!(e).into())
        })
}

#[get("/<keyspace>/messages/<message_id>/metadata")]
async fn get_message_metadata(
    keyspace: String,
    message_id: String,
    keyspaces: &State<HashSet<String>>,
) -> ListenerResult {
    if !keyspaces.contains(&keyspace) {
        return Err(ListenerError::InvalidKeyspace(keyspace));
    }
    let keyspace = ChronicleKeyspace::new(keyspace);
    let message_id = Bee(MessageId::from_str(&message_id).map_err(|e| ListenerError::BadParse(e.into()))?);
    query::<Option<MessageMetadata>, _, _, _>(keyspace, message_id, (), None, None)
        .await
        .and_then(|o| o.ok_or(ListenerError::NotFound))
        .map(|metadata| metadata.into())
}

#[get("/<keyspace>/messages/<message_id>/children?<page_size>&<expanded>&<state>")]
async fn get_message_children(
    keyspace: String,
    message_id: String,
    page_size: Option<usize>,
    expanded: Option<bool>,
    state: Option<String>,
    partition_config: &State<PartitionConfig>,
    keyspaces: &State<HashSet<String>>,
) -> ListenerResult {
    if !keyspaces.contains(&keyspace) {
        return Err(ListenerError::InvalidKeyspace(keyspace));
    }
    let message_id = Bee(MessageId::from_str(&message_id).map_err(|e| ListenerError::BadParse(e.into()))?);
    let page_size = page_size.unwrap_or(100);

    let mut state = state
        .map(|state| {
            hex::decode(state)
                .map_err(|_| ListenerError::InvalidState)
                .and_then(|v| bincode::deserialize::<StateData>(&v).map_err(|_| ListenerError::InvalidState))
        })
        .transpose()?;

    let mut messages = page(
        keyspace.clone(),
        Hint::parent(message_id.to_string()),
        page_size,
        &mut state,
        partition_config.borrow(),
        message_id,
    )
    .await?;

    let state = state
        .map(|state| bincode::serialize(&state).map(|v| hex::encode(v)))
        .transpose()
        .map_err(|e| anyhow!(e))?;

    if let Some(true) = expanded {
        Ok(ListenerResponse::MessageChildrenExpanded {
            message_id: message_id.to_string(),
            max_results: 2 * page_size,
            count: messages.len(),
            children_message_ids: messages.drain(..).map(|record| record.into()).collect(),
            state,
        })
    } else {
        Ok(ListenerResponse::MessageChildren {
            message_id: message_id.to_string(),
            max_results: 2 * page_size,
            count: messages.len(),
            children_message_ids: messages.drain(..).map(|record| record.message_id.to_string()).collect(),
            state,
        })
    }
}

#[get("/<keyspace>/messages?<index>&<page_size>&<utf8>&<expanded>&<state>")]
async fn get_message_by_index(
    keyspace: String,
    mut index: String,
    page_size: Option<usize>,
    utf8: Option<bool>,
    expanded: Option<bool>,
    state: Option<String>,
    partition_config: &State<PartitionConfig>,
    keyspaces: &State<HashSet<String>>,
) -> ListenerResult {
    if !keyspaces.contains(&keyspace) {
        return Err(ListenerError::InvalidKeyspace(keyspace));
    }
    if let Some(true) = utf8 {
        index = hex::encode(index);
    }
    if Vec::<u8>::from_hex(index.clone())
        .map_err(|_| ListenerError::InvalidHex)?
        .len()
        > 64
    {
        return Err(ListenerError::IndexTooLarge);
    }

    let mut state = state
        .map(|state| {
            hex::decode(state)
                .map_err(|_| ListenerError::InvalidState)
                .and_then(|v| bincode::deserialize::<StateData>(&v).map_err(|_| ListenerError::InvalidState))
        })
        .transpose()?;

    let indexation = Indexation(index.clone());
    let page_size = page_size.unwrap_or(1000);

    let mut messages = page(
        keyspace.clone(),
        Hint::index(index.clone()),
        page_size,
        &mut state,
        partition_config.borrow(),
        indexation,
    )
    .await?;

    let state = state
        .map(|state| bincode::serialize(&state).map(|v| hex::encode(v)))
        .transpose()
        .map_err(|e| anyhow!(e))?;

    if let Some(true) = expanded {
        Ok(ListenerResponse::MessagesForIndexExpanded {
            index,
            max_results: 2 * page_size,
            count: messages.len(),
            message_ids: messages.drain(..).map(|record| record.into()).collect(),
            state,
        })
    } else {
        Ok(ListenerResponse::MessagesForIndex {
            index,
            max_results: 2 * page_size,
            count: messages.len(),
            message_ids: messages.drain(..).map(|record| record.message_id.to_string()).collect(),
            state,
        })
    }
}

#[get("/<keyspace>/addresses/ed25519/<address>/outputs?<page_size>&<expanded>&<state>")]
async fn get_ed25519_outputs(
    keyspace: String,
    address: String,
    page_size: Option<usize>,
    expanded: Option<bool>,
    state: Option<String>,
    partition_config: &State<PartitionConfig>,
    keyspaces: &State<HashSet<String>>,
) -> ListenerResult {
    if !keyspaces.contains(&keyspace) {
        return Err(ListenerError::InvalidKeyspace(keyspace));
    }
    let mut state = state
        .map(|state| {
            hex::decode(state)
                .map_err(|_| ListenerError::InvalidState)
                .and_then(|v| bincode::deserialize::<StateData>(&v).map_err(|_| ListenerError::InvalidState))
        })
        .transpose()?;

    let ed25519_address = Bee(Ed25519Address::from_str(&address).map_err(|e| ListenerError::BadParse(e.into()))?);
    let page_size = page_size.unwrap_or(100);

    let mut outputs = page(
        keyspace.clone(),
        Hint::address(ed25519_address.to_string()),
        page_size,
        &mut state,
        partition_config.borrow(),
        ed25519_address,
    )
    .await?;

    let state = state
        .map(|state| bincode::serialize(&state).map(|v| hex::encode(v)))
        .transpose()
        .map_err(|e| anyhow!(e))?;

    if let Some(true) = expanded {
        Ok(ListenerResponse::OutputsForAddressExpanded {
            address_type: 1,
            address,
            max_results: 2 * page_size,
            count: outputs.len(),
            output_ids: outputs
                .drain(..)
                .map(|record| Ok(record.try_into()?))
                .filter_map(|r: anyhow::Result<responses::Record>| r.ok())
                .collect(),
            state,
        })
    } else {
        Ok(ListenerResponse::OutputsForAddress {
            address_type: 1,
            address,
            max_results: 2 * page_size,
            count: outputs.len(),
            output_ids: outputs
                .drain(..)
                .map(|record| Ok(OutputId::new(record.transaction_id, record.index)?))
                .filter_map(|r: anyhow::Result<OutputId>| r.ok())
                .collect(),
            state,
        })
    }
}

#[get("/<keyspace>/outputs/<transaction_id>/<idx>")]
async fn get_output_by_transaction_id(
    keyspace: String,
    transaction_id: String,
    idx: u16,
    keyspaces: &State<HashSet<String>>,
) -> ListenerResult {
    get_output(
        keyspace,
        TransactionId::from_str(&transaction_id)
            .and_then(|t| OutputId::new(t, idx))
            .map_err(|e| ListenerError::BadParse(e.into()))?
            .to_string(),
        keyspaces,
    )
    .await
}

#[get("/<keyspace>/outputs/<output_id>")]
async fn get_output(keyspace: String, output_id: String, keyspaces: &State<HashSet<String>>) -> ListenerResult {
    if !keyspaces.contains(&keyspace) {
        return Err(ListenerError::InvalidKeyspace(keyspace));
    }
    let (transaction_id, index) = OutputId::from_str(&output_id)
        .map_err(|e| ListenerError::BadParse(e.into()))?
        .split();

    let output_data = query::<OutputRes, _, _, _>(
        ChronicleKeyspace::new(keyspace.clone()),
        Bee(transaction_id.clone()),
        index,
        None,
        None,
    )
    .await?;
    let is_spent = if output_data.unlock_blocks.is_empty() {
        false
    } else {
        let mut is_spent = false;
        let mut query_message_ids = HashSet::new();
        for UnlockRes {
            message_id,
            block: _,
            inclusion_state,
        } in output_data.unlock_blocks.iter()
        {
            if *inclusion_state == Some(LedgerInclusionState::Included) {
                is_spent = true;
                break;
            } else {
                query_message_ids.insert(message_id);
            }
        }
        if !query_message_ids.is_empty() {
            let queries = query_message_ids.drain().map(|&message_id| {
                query::<Option<MessageMetadata>, _, _, _>(
                    ChronicleKeyspace::new(keyspace.clone()),
                    Bee(message_id.clone()),
                    (),
                    None,
                    None,
                )
            });
            is_spent = futures::future::join_all(queries)
                .await
                .drain(..)
                .filter_map(|res| res.ok())
                .flatten()
                .any(|metadata| metadata.ledger_inclusion_state == Some(LedgerInclusionState::Included));
        }
        is_spent
    };
    Ok(ListenerResponse::Output {
        message_id: output_data.message_id.to_string(),
        transaction_id: transaction_id.to_string(),
        output_index: index,
        is_spent,
        output: output_data.output.borrow().into(),
    })
}

#[get("/<keyspace>/transactions/ed25519/<address>?<page_size>&<state>")]
async fn get_transactions_for_address(
    keyspace: String,
    address: String,
    page_size: Option<usize>,
    state: Option<String>,
    partition_config: &State<PartitionConfig>,
    keyspaces: &State<HashSet<String>>,
) -> ListenerResult {
    if !keyspaces.contains(&keyspace) {
        return Err(ListenerError::InvalidKeyspace(keyspace));
    }
    let mut state = state
        .map(|state| {
            hex::decode(state)
                .map_err(|_| ListenerError::InvalidState)
                .and_then(|v| bincode::deserialize::<StateData>(&v).map_err(|_| ListenerError::InvalidState))
        })
        .transpose()?;

    let ed25519_address = Bee(Ed25519Address::from_str(&address).map_err(|e| ListenerError::BadParse(e.into()))?);
    let page_size = page_size.unwrap_or(100);

    let outputs = page(
        keyspace.clone(),
        Hint::address(ed25519_address.to_string()),
        page_size,
        &mut state,
        partition_config.borrow(),
        ed25519_address,
    )
    .await?;

    let transactions = futures::stream::iter(outputs)
        .map(|o| (o, keyspace.clone()))
        .then(|(o, keyspace)| async move {
            query::<TransactionRes, _, _, _>(ChronicleKeyspace::new(keyspace), Bee(o.transaction_id), (), None, None)
                .await
                .map(Into::into)
        })
        .try_collect()
        .await?;

    let state = state
        .map(|state| bincode::serialize(&state).map(|v| hex::encode(v)))
        .transpose()
        .map_err(|e| anyhow!(e))?;

    Ok(ListenerResponse::Transactions { transactions, state })
}

#[get("/<keyspace>/transactions/<message_id>")]
async fn get_transaction_for_message(
    keyspace: String,
    message_id: String,
    keyspaces: &State<HashSet<String>>,
) -> ListenerResult {
    if !keyspaces.contains(&keyspace) {
        return Err(ListenerError::InvalidKeyspace(keyspace));
    }
    let keyspace = ChronicleKeyspace::new(keyspace);
    let message_id = Bee(MessageId::from_str(&message_id).map_err(|e| ListenerError::BadParse(e.into()))?);
    let message = query::<Bee<Message>, _, _, _>(keyspace.clone(), message_id, (), None, None).await?;
    let transaction_id = if let Some(payload) = message.payload() {
        match payload {
            Payload::Transaction(p) => p.id(),
            _ => return Err(ListenerError::NoResults),
        }
    } else {
        return Err(ListenerError::NoResults);
    };
    let transaction = query::<TransactionRes, _, _, _>(keyspace, Bee(transaction_id), (), None, None).await?;
    Ok(ListenerResponse::Transaction(transaction.into()))
}

#[get("/<keyspace>/transactions/<transaction_id>/included-message")]
async fn get_transaction_included_message(
    keyspace: String,
    transaction_id: String,
    keyspaces: &State<HashSet<String>>,
) -> ListenerResult {
    if !keyspaces.contains(&keyspace) {
        return Err(ListenerError::InvalidKeyspace(keyspace));
    }
    let keyspace = ChronicleKeyspace::new(keyspace);

    let transaction_id = Bee(TransactionId::from_str(&transaction_id).map_err(|e| ListenerError::Other(anyhow!(e)))?);

    let message_id = query::<Option<Bee<MessageId>>, _, _, _>(
        keyspace.clone(),
        transaction_id,
        LedgerInclusionState::Included,
        None,
        None,
    )
    .await?
    .ok_or_else(|| ListenerError::NoResults)?;
    query::<Bee<Message>, _, _, _>(keyspace, message_id, (), None, None)
        .await
        .and_then(|message| {
            message
                .into_inner()
                .try_into()
                .map_err(|e: Cow<'static, str>| anyhow!(e).into())
        })
}

#[get("/<keyspace>/milestones/<index>")]
async fn get_milestone(keyspace: String, index: u32, keyspaces: &State<HashSet<String>>) -> ListenerResult {
    if !keyspaces.contains(&keyspace) {
        return Err(ListenerError::InvalidKeyspace(keyspace));
    }
    let keyspace = ChronicleKeyspace::new(keyspace);

    query::<Bee<Milestone>, _, _, _>(keyspace, Bee(MilestoneIndex::from(index)), (), None, None)
        .await
        .map(|milestone| ListenerResponse::Milestone {
            milestone_index: index,
            message_id: milestone.message_id().to_string(),
            timestamp: milestone.timestamp(),
        })
}

#[get("/<keyspace>/analytics?<start>&<end>")]
async fn get_analytics(
    keyspace: String,
    start: Option<u32>,
    end: Option<u32>,
    keyspaces: &State<HashSet<String>>,
) -> ListenerResult {
    if !keyspaces.contains(&keyspace) {
        return Err(ListenerError::InvalidKeyspace(keyspace));
    }
    let keyspace = ChronicleKeyspace::new(keyspace);

    let range = start.unwrap_or(1)..end.unwrap_or(i32::MAX as u32);
    let range = SyncRange::try_from(range).map_err(|e| ListenerError::BadParse(e))?;
    let ranges = AnalyticsData::try_fetch(&keyspace, &range, 1, 5000).await?.analytics;
    Ok(ListenerResponse::Analytics { ranges })
}

#[catch(500)]
fn internal_error() -> ListenerError {
    ListenerError::Other(anyhow!("Internal server error!"))
}

#[catch(404)]
fn not_found() -> ListenerError {
    ListenerError::NotFound
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::rocket::{
        http::{
            ContentType,
            Header,
            Status,
        },
        local::asynchronous::{
            Client,
            LocalResponse,
        },
    };
    use serde_json::Value;

    fn check_cors_headers(res: &LocalResponse) {
        assert_eq!(
            res.headers().get_one("Access-Control-Allow-Origin"),
            Some(Header::new("Access-Control-Allow-Origin", "*").value())
        );
        assert_eq!(
            res.headers().get_one("Access-Control-Allow-Methods"),
            Some(Header::new("Access-Control-Allow-Methods", "GET, OPTIONS").value())
        );
        assert_eq!(
            res.headers().get_one("Access-Control-Allow-Headers"),
            Some(Header::new("Access-Control-Allow-Headers", "*").value())
        );
        assert_eq!(
            res.headers().get_one("Access-Control-Allow-Credentials"),
            Some(Header::new("Access-Control-Allow-Credentials", "true").value())
        );
    }

    async fn construct_client() -> Client {
        let mut keyspaces = HashSet::new();
        keyspaces.insert("permanode".to_string());
        let rocket = construct_rocket().manage(PartitionConfig::default()).manage(keyspaces);
        Client::tracked(rocket).await.expect("Invalid rocket instance!")
    }

    #[::rocket::async_test]
    async fn options() {
        let client = construct_client().await;

        let res = client.options("/api/anything").dispatch().await;
        assert_eq!(res.status(), Status::Ok);
        assert_eq!(res.content_type(), None);
        check_cors_headers(&res);
        assert!(res.into_string().await.is_none());
    }

    #[::rocket::async_test]
    async fn info() {
        let client = construct_client().await;

        let res = client.get("/api/permanode/info").dispatch().await;
        assert_eq!(res.status(), Status::Ok);
        assert_eq!(res.content_type(), Some(ContentType::JSON));
        check_cors_headers(&res);
        let body: SuccessBody<ListenerResponse> =
            serde_json::from_str(&res.into_string().await.expect("No body returned!"))
                .expect("Failed to deserialize Info Response!");
        match *body {
            ListenerResponse::Info { .. } => (),
            _ => panic!("Did not receive an info response!"),
        }
    }

    #[::rocket::async_test]
    async fn service() {
        let client = construct_client().await;

        let res = client.get("/api/service").dispatch().await;
        assert_eq!(res.status(), Status::Ok);
        assert_eq!(res.content_type(), Some(ContentType::JSON));
        check_cors_headers(&res);
        // let _body: Service = serde_json::from_str(&res.into_string().await.expect("No body returned!"))
        //    .expect("Failed to deserialize Service Tree Response!");
    }

    #[::rocket::async_test]
    async fn get_message() {
        let client = construct_client().await;

        let res = client
            .get("/api/permanode/messages/91515c13d2025f79ded3758abe5dc640591c3b6d58b1c52cd51d1fa0585774bc")
            .dispatch()
            .await;
        assert_eq!(res.status(), Status::InternalServerError);
        assert_eq!(res.content_type(), Some(ContentType::JSON));
        check_cors_headers(&res);
        let body: Value = serde_json::from_str(&res.into_string().await.expect("No body returned!"))
            .expect("Failed to deserialize response!");
        assert_eq!(body.get("message").and_then(Value::as_str), Some("Worker NoRing"));
    }
}
