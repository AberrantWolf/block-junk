-- Lists remain replacements; this balance edit touches one scalar field.
engine.recipes.patch {id="vanilla:planks_from_log", duration_secs=2.0}
-- This kind explicitly opts out of hauling. Candidate enumeration therefore
-- cannot offer haul work before or after policy runs.
engine.npcs.patch {
    id="vanilla:wanderer",
    job_classes={"construction","crafting"},
}
