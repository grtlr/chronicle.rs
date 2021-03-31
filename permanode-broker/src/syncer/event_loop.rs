// Copyright 2021 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use super::*;
#[async_trait::async_trait]
impl<H: PermanodeBrokerScope> EventLoop<BrokerHandle<H>> for Syncer {
    async fn event_loop(
        &mut self,
        _status: Result<(), Need>,
        _supervisor: &mut Option<BrokerHandle<H>>,
    ) -> Result<(), Need> {
        while let Some(event) = self.inbox.recv().await {
            match event {
                SyncerEvent::Process => {
                    self.process_more();
                }
                SyncerEvent::Ask(ask) => {
                    // Don't accept ask events when there is something already in progress.
                    if let None = self.active {
                        match ask {
                            AskSyncer::Complete => {
                                self.complete();
                            }
                            AskSyncer::FillGaps => {
                                self.fill_gaps();
                            }
                            AskSyncer::UpdateSyncData => {
                                todo!("Updating the sync data is not implemented yet")
                            }
                        }
                    } else {
                        error!(
                            "Cannot accept Ask request: {:?}, while processing: {:?}",
                            &ask, self.active
                        );
                    }
                }
                SyncerEvent::MilestoneData(milestone_data) => {
                    self.handle_milestone_data(milestone_data).await;
                }
                SyncerEvent::Shutdown => break,
            }
        }
        Ok(())
    }
}

impl Syncer {
    pub(crate) async fn handle_milestone_data(&mut self, milestone_data: MilestoneData) {
        self.pending -= 1;
        self.milestones_data.push(Ascending::new(milestone_data));
        if self.highest.eq(&0) && self.pending.eq(&0) {
            // these are the first milestones data, which we didn't even request it.
            let milestone_data = self.milestones_data.pop().unwrap().into_inner();
            self.highest = milestone_data.milestone_index();
            let mut next = self.highest + 1;
            // push it to archiver
            let _ = self.archiver_handle.send(ArchiverEvent::MilestoneData(milestone_data));
            // push the rest
            while let Some(ms_data) = self.milestones_data.pop() {
                let milestone_data = ms_data.into_inner();
                let ms_index = milestone_data.milestone_index();
                if next != ms_index {
                    // identify self.highest as glitch.
                    // eventually we will fill up this glitch
                    warn!(
                        "Noticed a glitch: {}..{} in the first observed milestones data",
                        self.highest + 1,
                        ms_index,
                    );
                    // we update our highest to be the ms_index which caused the glitch
                    // this enable us later to solidify the last gap up to this ms.
                    self.highest = ms_index;
                }
                next = ms_index + 1;
                // push it to archiver
                let _ = self.archiver_handle.send(ArchiverEvent::MilestoneData(milestone_data));
            }
        } else if !self.highest.eq(&0) {
            // check if we could send the next expected milestone_index
            while let Some(ms_data) = self.milestones_data.pop() {
                let ms_index = ms_data.get_ref().milestone_index();
                if self.next.eq(&ms_index) {
                    // push it to archiver
                    let _ = self
                        .archiver_handle
                        .send(ArchiverEvent::MilestoneData(ms_data.into_inner()));
                    self.next += 1;
                } else {
                    // put it back and then break
                    self.milestones_data.push(ms_data);
                    break;
                }
            }
        }
        // check if pending is zero which is an indicator that all milestones_data
        // has been processed, in order to move further
        self.trigger_process_more();
    }
    pub(crate) fn process_more(&mut self) {
        if let Some(ref mut active) = self.active {
            match active {
                Active::Complete(range) => {
                    for _ in 0..self.solidifier_count {
                        if let Some(milestone_index) = range.next() {
                            Self::request_solidify(self.solidifier_count, &self.solidifier_handles, milestone_index);
                            // update pending
                            self.pending += 1;
                        } else {
                            // move to next gap (only if pending is zero)
                            if self.pending.eq(&0) {
                                // Finished the current active range, therefore we drop it
                                self.active.take();
                                self.complete();
                            }
                            break;
                        }
                    }
                }
                Active::FillGaps(range) => {
                    for _ in 0..self.solidifier_count {
                        if let Some(milestone_index) = range.next() {
                            Self::request_solidify(self.solidifier_count, &self.solidifier_handles, milestone_index);
                            // update pending
                            self.pending += 1;
                        } else {
                            // move to next gap (only if pending is zero)
                            if self.pending.eq(&0) {
                                // Finished the current active range, therefore we drop it
                                self.active.take();
                                self.fill_gaps();
                            }
                            break;
                        }
                    }
                }
            }
        } else {
            self.eof = true;
            info!("SyncData reached EOF")
        }
    }
    fn request_solidify(
        solidifier_count: u8,
        solidifier_handles: &HashMap<u8, SolidifierHandle>,
        milestone_index: u32,
    ) {
        let solidifier_id = (milestone_index % (solidifier_count as u32)) as u8;
        let solidifier_handle = solidifier_handles.get(&solidifier_id).unwrap();
        let solidify_event = SolidifierEvent::Solidify(milestone_index);
        let _ = solidifier_handle.send(solidify_event);
    }
    fn trigger_process_more(&mut self) {
        // move to next range (only if pending is zero)
        if self.pending.eq(&0) {
            // start processing it
            self.process_more();
        }
    }
    pub(crate) fn complete(&mut self) {
        // start from the lowest uncomplete
        if let Some(mut gap) = self.sync_data.take_lowest_uncomplete() {
            // ensure gap.end != i32::MAX
            if !gap.end.eq(&(i32::MAX as u32)) {
                info!("Completing the gap {:?}", gap);
                // set next to be the start
                self.next = gap.start;
                self.active.replace(Active::Complete(gap));
                self.trigger_process_more();
            } else {
                // fill this with the gap.start up to self.highest
                // this is the last gap in our sync data
                // First we ensure highest is larger than gap.start
                if self.highest > gap.start {
                    info!("Completing the last gap {:?}", gap);
                    // set next to be the start
                    self.next = gap.start;
                    // update the end of the gap
                    gap.end = self.highest;
                    self.active.replace(Active::Complete(gap));
                    self.trigger_process_more();
                } else {
                    info!("There are no more gaps neither unlogged in the current sync data")
                }
            }
        } else {
            info!("There are no more gaps neither unlogged in the current sync data");
        }
    }
    pub(crate) fn fill_gaps(&mut self) {
        // start from the lowest gap
        if let Some(mut gap) = self.sync_data.take_lowest_gap() {
            // ensure gap.end != i32::MAX
            if !gap.end.eq(&(i32::MAX as u32)) {
                info!("Filling the gap {:?}", gap);
                // set next to be the start
                self.next = gap.start;
                self.active.replace(Active::FillGaps(gap));
                self.trigger_process_more();
            } else {
                // fill this with the gap.start up to self.highest
                // this is the last gap in our sync data
                // First we ensure highest is larger than gap.start
                if self.highest > gap.start {
                    info!("Filling the last gap {:?}", gap);
                    // set next to be the start
                    self.next = gap.start;
                    // update the end of the gap
                    gap.end = self.highest;
                    self.active.replace(Active::FillGaps(gap));
                    self.trigger_process_more();
                } else {
                    info!("There are no more gaps in the current sync data")
                }
            }
        } else {
            info!("There are no more gaps in the current sync data");
        }
    }
}
