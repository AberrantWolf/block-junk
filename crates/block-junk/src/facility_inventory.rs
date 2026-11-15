//! Inventory lifetime follows the authoritative edit transaction, never a delayed
//! cell observer that might accidentally empty a replacement at the same anchor.
use bevy::prelude::*;
use std::collections::BTreeMap;

pub(crate) struct CraftRequest<'a> {
    pub cause: Option<block_junk_mod_api::authority::EventCause>,
    pub anchor: IVec3,
    pub actor: block_junk_mod_api::authority::ReservationOwner,
    pub controller: block_junk_mod_api::authority::ReservationOwner,
    pub order_id: u64,
    pub order_revision: u64,
    pub tool: Option<crate::items::ItemSlot>,
    pub station: &'a crate::craft_stations::StationDefView,
    pub recipes: &'a crate::recipes::RecipeRegistry,
}

#[derive(Default)]
pub(crate) struct Banks {
    pub storage: crate::containers::ContainerState,
    pub crafting: crate::craft_stations::StationState,
}
#[derive(Default)]
pub(crate) struct FacilityTransaction {
    bookings: Option<crate::craft_stations::CraftBookings>,
    order_ids: Option<crate::craft_stations::CraftOrderIds>,
    banks: BTreeMap<[i32; 3], Banks>,

    pending: crate::item_placement::ItemPlacementQueue,
}

#[derive(bevy::ecs::system::SystemParam)]
pub(crate) struct FacilityService<'w> {
    pub spatial_events: ResMut<'w, crate::spatial_events::SpatialJournal>,
    order_ids: ResMut<'w, crate::craft_stations::CraftOrderIds>,
    storage: ResMut<'w, crate::containers::Containers>,
    crafting: ResMut<'w, crate::craft_stations::CraftStations>,
    bookings: ResMut<'w, crate::craft_stations::CraftBookings>,
    pending: ResMut<'w, crate::item_placement::ItemPlacementQueue>,
    items: Res<'w, crate::items::ItemRegistry>,
}
impl FacilityTransaction {
    pub fn relocate(
        &mut self,
        from: IVec3,
        to: IVec3,
        service: &FacilityService,
    ) -> Result<(), String> {
        if from != to {
            let destination = self.at(to, service);
            if !destination.storage.is_empty() || !destination.crafting.is_empty() {
                return Err("destination has facility inventory or work".into());
            }
            let banks = std::mem::take(self.at(from, service));
            *self.at(to, service) = banks;
        }
        // Durable stock/orders/inputs move; transient leases do not.
        self.booking_view(service).force_clear_worker(from);
        Ok(())
    }
    pub fn consume_stock(
        &mut self,
        anchor: IVec3,
        count: u32,
        tag: Option<&block_junk_mod_api::blocks::TagId>,
        service: &FacilityService,
    ) -> Result<(), String> {
        let storage = &mut self.at(anchor, service).storage;
        let mut candidates: Vec<_> = storage
            .inventory
            .iter()
            .filter(|(item, n)| {
                **n > 0 && tag.is_none_or(|tag| service.items.def(**item).tags.contains(tag))
            })
            .map(|(item, n)| (*item, *n))
            .collect();
        candidates.sort_by_key(|(item, _)| service.items.id_of(*item).as_str());
        if count == 0
            || candidates.iter().map(|(_, n)| u64::from(*n)).sum::<u64>() < u64::from(count)
        {
            return Err("facility lacks the required consumable stock".into());
        }
        let mut remaining = count;
        for (item, n) in candidates {
            let take = n.min(remaining);
            storage.withdraw_up_to(item, take);
            remaining -= take;
            if remaining == 0 {
                break;
            }
        }
        Ok(())
    }
    pub fn actor_has_job(&self, id: u64, service: &FacilityService) -> bool {
        self.bookings
            .as_ref()
            .unwrap_or(&service.bookings)
            .jobs
            .records
            .values()
            .any(|job| {
                !job.state.terminal()
                    && job.actor == block_junk_mod_api::authority::ReservationOwner::Npc { id }
            })
    }
    fn booking_view(
        &mut self,
        service: &FacilityService,
    ) -> &mut crate::craft_stations::CraftBookings {
        self.bookings
            .get_or_insert_with(|| service.bookings.clone())
    }
    pub fn read_job(
        &self,
        id: u64,
        service: &FacilityService,
    ) -> Result<block_junk_mod_api::authority::JobRecord, String> {
        self.bookings
            .as_ref()
            .unwrap_or(&service.bookings)
            .jobs
            .records
            .get(&id)
            .cloned()
            .ok_or_else(|| "craft job missing or already retired".into())
    }
    pub fn acknowledge_job(
        &mut self,
        owner: &str,
        id: u64,
        revision: u64,
        service: &FacilityService,
    ) -> Result<(), String> {
        let job = self.read_job(id, service)?;
        if !matches!(&job.controller, block_junk_mod_api::authority::ReservationOwner::Behavior { mod_id, .. } if mod_id == owner)
        {
            return Err("only the controlling mod may acknowledge a job result".into());
        }
        self.booking_view(service).jobs.acknowledge(id, revision)
    }
    pub fn owned_jobs(
        &self,
        owner: &str,
        after: u64,
        limit: usize,
        service: &FacilityService,
    ) -> (Vec<block_junk_mod_api::authority::JobRecord>, bool) {
        self.bookings
            .as_ref()
            .unwrap_or(&service.bookings)
            .jobs
            .owned_jobs(owner, after, limit)
    }
    pub fn release_actor(&mut self, id: crate::npc::NpcId, service: &FacilityService) {
        self.booking_view(service).fail_actor(id);
    }
    pub fn prepare_craft(
        &mut self,
        service: &FacilityService,
        request: CraftRequest<'_>,
    ) -> Result<block_junk_mod_api::authority::JobRecord, String> {
        let CraftRequest {
            cause,
            anchor,
            actor,
            controller,
            order_id,
            order_revision,
            tool,
            station,
            recipes,
        } = request;
        let mut state = self.at(anchor, service).crafting.clone();
        state.prepare_order_work(
            order_id,
            order_revision,
            station,
            tool,
            &service.items,
            recipes,
        )?;
        let bookings = self.booking_view(service);
        let before = bookings.jobs.next;
        let id = bookings.prepare_job(anchor, actor, controller, order_id, order_revision)?;
        let job = bookings.jobs.records.get_mut(&id).expect("accepted job");
        if id == before {
            job.cause = cause;
        }
        let job = job.clone();
        self.at(anchor, service).crafting = state;
        Ok(job)
    }
    pub fn return_item(
        &mut self,
        origin: IVec3,
        item: block_junk_mod_api::items::ItemId,
        count: u32,
    ) {
        self.pending.enqueue(origin, item, count);
    }
    pub fn at(&mut self, anchor: IVec3, service: &FacilityService) -> &mut Banks {
        self.banks
            .entry(anchor.to_array())
            .or_insert_with(|| Banks {
                storage: service.storage.get(anchor).cloned().unwrap_or_default(),
                crafting: service.crafting.get(anchor).cloned().unwrap_or_default(),
            })
    }
    pub fn queue_order(
        &mut self,
        anchor: IVec3,
        service: &FacilityService,
        recipes: &crate::recipes::RecipeRegistry,
        station: &crate::craft_stations::StationDefView,
        recipe_id: &str,
        quantity: u32,
    ) -> Result<u64, String> {
        let mut ids = self
            .order_ids
            .clone()
            .unwrap_or_else(|| service.order_ids.clone());
        let id = ids.queue(
            &mut self.at(anchor, service).crafting,
            recipes,
            station,
            recipe_id,
            quantity,
        )?;
        self.order_ids = Some(ids);
        Ok(id)
    }
    pub fn cancel_order(
        &mut self,
        anchor: IVec3,
        service: &FacilityService,
        id: u64,
        revision: u64,
    ) -> Result<(), String> {
        if self
            .at(anchor, service)
            .crafting
            .cancel_order(id, revision)?
        {
            self.booking_view(service).force_clear_station(anchor);
        }
        Ok(())
    }
    pub fn retire(
        &mut self,
        effects: &[crate::protocol::TerrainEditRecord],
        blocks: &crate::blocks::BlockRegistry,
        service: &FacilityService,
    ) {
        for effect in effects {
            let old = blocks.def(effect.old_slot);
            if old.container.is_none() && old.station_tag.is_none() {
                continue;
            }
            let banks = self.at(effect.anchor, service);
            let mut bundle = Vec::new();
            if old.container.is_some() {
                bundle.extend(std::mem::take(&mut banks.storage).inventory);
            }
            if old.station_tag.is_some() {
                let station = std::mem::take(&mut banks.crafting);
                bundle.extend(station.inventory);
                bundle.extend(
                    station
                        .active_work
                        .into_iter()
                        .flat_map(|work| work.committed_inputs),
                );
            }
            bundle.sort_by_key(|(item, _)| item.0);
            for (item, count) in bundle {
                self.pending
                    .enqueue(effect.anchor, service.items.id_of(item).clone(), count);
            }
            self.booking_view(service).fail_station(effect.anchor);
        }
    }
}
impl FacilityService<'_> {
    pub fn retire(
        &mut self,
        effects: &[crate::protocol::TerrainEditRecord],
        blocks: &crate::blocks::BlockRegistry,
    ) {
        let mut transaction = FacilityTransaction::default();
        transaction.retire(effects, blocks, self);
        self.commit(transaction);
    }
    pub fn commit(&mut self, transaction: FacilityTransaction) {
        if let Some(bookings) = transaction.bookings {
            *self.bookings = bookings;
        }
        if let Some(ids) = transaction.order_ids {
            *self.order_ids = ids;
        }
        for (anchor, banks) in transaction.banks {
            let cell = IVec3::from_array(anchor);
            if self.storage.get(cell) != Some(&banks.storage)
                && (!banks.storage.is_empty() || self.storage.get(cell).is_some())
            {
                self.storage.upsert(cell, banks.storage, 0);
                self.storage.remove_if_empty(cell);
            }
            if self.crafting.get(cell) != Some(&banks.crafting)
                && (!banks.crafting.is_empty() || self.crafting.get(cell).is_some())
            {
                self.crafting.upsert(cell, banks.crafting, 0);
                self.crafting.remove_if_empty(cell);
            }
        }
        self.pending.0.extend(transaction.pending.0);
    }
}

#[cfg(test)]
pub fn install_fixture(app: &mut App) {
    app.init_resource::<crate::spatial_events::SpatialJournal>();
    app.init_resource::<crate::space_identity::SpaceIdentities>();
    app.init_resource::<crate::facility_interactions::FacilityInteractions>();
    app.init_resource::<crate::interactables::InteractionClaims>()
        .init_resource::<crate::npc::NpcIdAllocator>()
        .init_resource::<crate::craft_stations::CraftOrderIds>()
        .init_resource::<crate::containers::Containers>()
        .init_resource::<crate::craft_stations::CraftStations>()
        .init_resource::<crate::craft_stations::CraftBookings>()
        .init_resource::<crate::item_placement::ItemPlacementQueue>();
    if !app
        .world()
        .contains_resource::<crate::items::ItemRegistry>()
    {
        app.insert_resource(crate::items::ItemRegistry::build(Vec::new()).unwrap());
    }
}
