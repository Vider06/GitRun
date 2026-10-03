            "installed update requires at least one artifact".into(),
        ));
    }
    for artifact in artifacts {
        if artifact.archive_name.is_empty()
            || artifact.archive_name.contains(['/', '\\\\'])
            || artifact.archive_name == "."
            || artifact.archive_name == ".."
        {