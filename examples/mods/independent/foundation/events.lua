engine.npcs.set_planner("foundation:settler",function(snapshot)
    if snapshot.job_candidates and #snapshot.job_candidates > 0 then
        local job = snapshot.job_candidates[1]
        return {kind="take_job",candidate_id=job.id,revision=job.revision}
    end
    return {kind="wander",radius_cells=4,timeout_secs=4}
end)
