use std::path::Path;

const RETIRED_SPATIAL_TYPES: &[&str] = &[
    "AppliedBlockEdit",
    "ChunkSnapshot",
    "ChunkUnload",
    "PlanFullSync",
    "StorageFullSync",
    "ContainersFullSync",
    "StationsFullSync",
    "RoomsFullSync",
    "ContainerUpdate",
    "StationUpdate",
];

pub fn violations(path: &Path, source: &str) -> Vec<String> {
    let mut result = Vec::new();
    let source: String = source
        .lines()
        .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n");
    if !path.ends_with("spatial.rs") {
        for forbidden in [
            "Replicate::to_clients(",
            ".gain_visibility(",
            ".lose_visibility(",
            "NetworkTarget::All",
        ] {
            if source.contains(forbidden) {
                result.push(format!("{forbidden} is restricted to spatial.rs"));
            }
        }
    }
    if !path.ends_with("network.rs")
        && source.contains("register_message::<")
        && source.contains("NetworkDirection::ServerToClient")
    {
        result.push("server-to-client message registration is restricted to network.rs".into());
    }
    for retired in RETIRED_SPATIAL_TYPES {
        if source.contains(retired) {
            result.push(format!("retired spatial protocol type {retired}"));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_direct_replication_broadcast_and_visibility() {
        let fixture = r#"
            Replicate::to_clients(NetworkTarget::None);
            sender.send(&message, &NetworkTarget::All);
            commands.gain_visibility(entity, client);
            commands.lose_visibility(entity, client);
            app.register_message::<FeatureDelta>()
                .add_direction(NetworkDirection::ServerToClient);
            let _: ChunkSnapshot = value;
        "#;
        assert_eq!(violations(Path::new("server.rs"), fixture).len(), 6);
    }

    #[test]
    fn permits_framework_boundary_and_global_audience_name() {
        let fixture = r#"
            Replicate::to_clients(NetworkTarget::None);
            commands.gain_visibility(entity, client);
            GlobalAudience::target();
        "#;
        assert!(violations(Path::new("spatial.rs"), fixture).is_empty());
        assert!(violations(Path::new("global.rs"), "GlobalAudience::target()").is_empty());
    }
}
