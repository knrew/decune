use std::{fs, path::PathBuf, sync::OnceLock};

static FAKE_COMPOSE_CAPABILITIES_SCRIPT_PATH: OnceLock<PathBuf> = OnceLock::new();

const FAKE_COMPOSE_CAPABILITIES_SCRIPT: &str = r#"case " $* " in
  *" version --short "*)
    printf '2.40.0\n'
    exit 0
    ;;
  *" version "*)
    printf 'Docker Compose version v2.40.0\n'
    exit 0
    ;;
  *" config --help "*)
    printf '%s\n' 'Usage: docker compose config [OPTIONS]' '      --format string'
    exit 0
    ;;
  *" ps --help "*)
    printf '%s\n' 'Usage: docker compose ps [OPTIONS]' '      --format string'
    exit 0
    ;;
  *" build --help "*)
    printf '%s\n' 'Usage: docker compose build [OPTIONS]' '      --with-dependencies'
    exit 0
    ;;
  *" pull --help "*)
    printf '%s\n' 'Usage: docker compose pull [OPTIONS]' '      --policy string' '      --ignore-buildable'
    exit 0
    ;;
  *" up --help "*)
    printf '%s\n' 'Usage: docker compose up [OPTIONS]' '      --force-recreate' '      --remove-orphans'
    exit 0
    ;;
esac
"#;

pub(crate) fn fake_compose_capabilities_script_path() -> PathBuf {
    FAKE_COMPOSE_CAPABILITIES_SCRIPT_PATH
        .get_or_init(|| {
            let root = std::env::temp_dir().join(format!(
                "decune-cli-test-compose-capabilities-{}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            let path = root.join("compose-capabilities.sh");
            fs::write(&path, FAKE_COMPOSE_CAPABILITIES_SCRIPT).unwrap();
            path
        })
        .clone()
}
