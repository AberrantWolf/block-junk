-- An explicit owner replaces the vanilla planner. It demonstrates three policy
-- decisions: rest wins over available work, sleeping facilities are selected by
-- capability, and only engine-enumerated candidates can become jobs.
engine.npcs.replace_planner("vanilla:wanderer", function(snapshot)
    if (snapshot.needs.sleep or 0) >= 0.6 then
        for _, facility in ipairs(snapshot.nearby_interactions or {}) do
            for _, capability in ipairs(facility.capabilities or {}) do
                if capability == "vanilla:sleeping" then
                    return { kind="interact", cell=facility.cell, timeout_secs=60 }
                end
            end
        end
    end
    if (snapshot.needs.work or 0) < 0.8 then
        return { kind = "rest", duration_secs = 4.0 }
    end
    if snapshot.job_candidates and #snapshot.job_candidates > 0 then
        local job = snapshot.job_candidates[1]
        return { kind="take_job", candidate_id=job.id, revision=job.revision }
    end
    return { kind = "rest", duration_secs = 4.0 }
end)
