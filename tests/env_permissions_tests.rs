#[cfg(unix)]
#[test]
fn boot_restricts_world_readable_env_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("llama-r-env-perms-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");
    std::fs::write(&env_path, "DEFAULT_MODEL=x").unwrap();
    std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    // Config::from_env lee del .env del cwd; el helper debe poder aplicarse
    // a una ruta explícita para ser testeable sin mutar el cwd del proceso.
    llama_r::config::restrict_env_permissions(&env_path);
    let mode = std::fs::metadata(&env_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    std::fs::remove_dir_all(&dir).unwrap();
}
