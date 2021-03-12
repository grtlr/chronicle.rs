// Copyright 2021 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use super::*;
use permanode_storage::access::worker::InsertWorker;

#[async_trait::async_trait]
impl<H: PermanodeBrokerScope> EventLoop<BrokerHandle<H>> for Collector {
    async fn event_loop(
        &mut self,
        _status: Result<(), Need>,
        _supervisor: &mut Option<BrokerHandle<H>>,
    ) -> Result<(), Need> {
        while let Some(event) = self.inbox.recv().await {
            match event {
                #[allow(unused_mut)]
                CollectorEvent::Message(message_id, mut message) => {
                    // info!("Inserting: {}", message_id.to_string());
                    // check if msg already in lru cache(if so then it's already presisted)
                    if let None = self.lru_msg.get(&message_id) {
                        {
                            self.default_keyspace
                                .insert(&message_id, &message)
                                .consistency(Consistency::One)
                                .build()
                                .send_local(Box::new(InsertWorker {
                                    keyspace: self.default_keyspace.clone(),
                                    key: message_id,
                                    value: message,
                                }));
                        }
                    } else {
                        // add it to the cache in order to not presist it again.
                        self.lru_msg.put(message_id, message);
                    }
                }
                CollectorEvent::MessageReferenced(msg_ref) => {
                    let partition_id = (msg_ref.referenced_by_milestone_index % (self.collectors_count as u64)) as u8;
                    let message_id = msg_ref.message_id;
                    // check if msg already in lru cache(if so then it's already presisted)
                    if let None = self.lru_msg_ref.get(&message_id) {
                        // TODO store it as metadata
                        // check if msg already exist in the cache, if so we pushed to solidifier
                    } else {
                        // add it to the cache in order to not presist it again.
                        self.lru_msg_ref.put(message_id, msg_ref);
                    }
                }
            }
        }
        Ok(())
    }
}
