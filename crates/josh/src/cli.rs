use std::process::ExitCode;

pub(crate) const DEFAULT_WALL_MS: u64 = 5_000;

#[derive(Default)]
pub(crate) struct RunOptions {
    pub(crate) entry: String,
    pub(crate) input: Option<String>,
    pub(crate) catalog: Option<String>,
    pub(crate) catalog_input: bool,
    pub(crate) executor: bool,
    pub(crate) granted_tools: Vec<String>,
    pub(crate) workdir: Option<String>,
    pub(crate) grants: Vec<String>,
    pub(crate) allowed_http_origins: Vec<String>,
    pub(crate) granted_exec: Vec<String>,
    pub(crate) granted_exec_environment: Vec<String>,
    pub(crate) wall_ms: Option<u64>,
    pub(crate) trace_events: bool,
    pub(crate) path: String,
}

pub(crate) fn print_help(program: &str) {
    eprintln!(
        "Usage:\n  {program} run [options] <source.allen|package-directory|artifact.allenb>\n  {program} serve\n\nOptions:\n  --entry <name>              Select an entry (default: main)\n  --input <json|@file|->      Supply exact entry JSON\n  --catalog <json-file>       Load a complete host tool catalog\n  --catalog-input             Use the frozen catalog result as entry input\n  --executor                  Enable the headless executor tool provider\n  --grant-tool <name>         Grant one exact catalog tool (repeatable)\n  --workdir <directory>       Select the sandbox working directory\n  --grant <capability>        Grant fs.read, fs.write, or net.http_get\n  --allow-net-origin <origin> Allow one canonical HTTPS origin\n  --grant-exec <pattern>      Grant one argv command pattern (repeatable)\n  --grant-exec-env <NAME>     Copy one requested host environment name\n  --wall-ms <milliseconds>    Set the wall-time limit (default: 5000)\n  --trace-events              Write JOSH execution events to stderr\n  -h, --help                  Show this help"
    );
}

#[allow(clippy::too_many_lines)]
pub(crate) fn parse_run(program: &str, arguments: &[String]) -> Result<Option<RunOptions>, String> {
    let mut options = RunOptions {
        entry: "main".to_owned(),
        wall_ms: Some(DEFAULT_WALL_MS),
        ..RunOptions::default()
    };
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "-h" | "--help" => {
                print_help(program);
                return Ok(None);
            }
            "--entry" => options.entry = take_value(arguments, &mut index, "--entry")?,
            "--input" => options.input = Some(take_value(arguments, &mut index, "--input")?),
            "--catalog" => {
                options.catalog = Some(take_value(arguments, &mut index, "--catalog")?);
            }
            "--catalog-input" => options.catalog_input = true,
            "--executor" => options.executor = true,
            "--grant-tool" => {
                options
                    .granted_tools
                    .push(take_value(arguments, &mut index, "--grant-tool")?);
            }
            "--workdir" => {
                options.workdir = Some(take_value(arguments, &mut index, "--workdir")?);
            }
            "--grant" => options
                .grants
                .push(take_value(arguments, &mut index, "--grant")?),
            "--allow-net-origin" => options.allowed_http_origins.push(take_value(
                arguments,
                &mut index,
                "--allow-net-origin",
            )?),
            "--grant-exec" => {
                options
                    .granted_exec
                    .push(take_value(arguments, &mut index, "--grant-exec")?);
            }
            "--grant-exec-env" => options.granted_exec_environment.push(take_value(
                arguments,
                &mut index,
                "--grant-exec-env",
            )?),
            "--wall-ms" => {
                let raw = take_value(arguments, &mut index, "--wall-ms")?;
                let value = raw
                    .parse::<u64>()
                    .ok()
                    .filter(|value| *value > 0)
                    .ok_or_else(|| "--wall-ms must be a positive integer".to_owned())?;
                options.wall_ms = Some(value);
            }
            "--trace-events" => options.trace_events = true,
            flag if flag.starts_with('-') => return Err(format!("unknown option '{flag}'")),
            path if options.path.is_empty() => path.clone_into(&mut options.path),
            path => return Err(format!("unexpected argument '{path}'")),
        }
        index += 1;
    }
    if options.path.is_empty() {
        return Err("run requires an ALLEN source file, package directory, or artifact".to_owned());
    }
    if options.entry.is_empty() {
        return Err("--entry cannot be empty".to_owned());
    }
    if options.catalog_input && options.input.is_some() {
        return Err("--catalog-input cannot be combined with --input".to_owned());
    }
    if !options.executor && !options.granted_tools.is_empty() {
        return Err("--grant-tool requires --executor".to_owned());
    }
    if !options.allowed_http_origins.is_empty() {
        options.grants.push("net.http_get".to_owned());
    }
    options.grants.sort();
    options.grants.dedup();
    options.granted_tools.sort();
    options.granted_tools.dedup();
    options.allowed_http_origins.sort();
    options.allowed_http_origins.dedup();
    options.granted_exec.sort();
    options.granted_exec.dedup();
    for pattern in &options.granted_exec {
        allen_exec::CommandPattern::parse(pattern)
            .map_err(|_| format!("--grant-exec pattern '{pattern}' is not canonical"))?;
    }
    options.granted_exec_environment.sort();
    options.granted_exec_environment.dedup();
    for name in &options.granted_exec_environment {
        let mut bytes = name.bytes();
        if !bytes.next().is_some_and(|first| {
            (first.is_ascii_alphabetic() || first == b'_')
                && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                && !name.eq_ignore_ascii_case("LC_ALL")
                && !name.eq_ignore_ascii_case("TZ")
        }) {
            return Err(format!("--grant-exec-env name '{name}' is not canonical"));
        }
    }
    Ok(Some(options))
}

pub(crate) fn argument_error(error: &str) -> ExitCode {
    eprintln!("josh: {error}");
    ExitCode::from(2)
}

fn take_value(arguments: &[String], index: &mut usize, option: &str) -> Result<String, String> {
    *index += 1;
    arguments
        .get(*index)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| format!("{option} requires a value"))
}
