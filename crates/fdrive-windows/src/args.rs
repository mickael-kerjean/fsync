use std::path::PathBuf;

use clap::Parser;
use fdrive_core::sdk::normalize_server;

use crate::gui::{self, Credentials};

#[derive(Parser)]
#[command(name = "fdrive-windows", about = "Filestash drive client — Windows")]
struct Args {
    #[arg(long, value_name = "URL")]
    server: Option<String>,
    #[arg(long, env = "FILESTASH_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[arg(long)]
    user: Option<String>,
    #[arg(long, env = "FILESTASH_PASSWORD", hide_env_values = true)]
    password: Option<String>,
    #[arg(long)]
    storage: Option<String>,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    insecure: bool,
    #[arg(long)]
    unregister: bool,
}

pub struct Setup {
    pub root: PathBuf,
    pub data: PathBuf,
    pub config_path: PathBuf,
    pub unregister: bool,
    pub prefill_url: Option<String>,
    pub credentials: Option<Credentials>,
    pub prompt_login: bool,
}

pub fn init() -> Result<Option<Setup>, Box<dyn std::error::Error>> {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(err)
            if matches!(
                err.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            gui::info(&err.to_string());
            return Ok(None);
        }
        Err(err) => {
            gui::alert(&err.to_string());
            std::process::exit(2);
        }
    };

    let root = PathBuf::from(std::env::var("USERPROFILE").map_err(|_| "no %USERPROFILE%")?)
        .join("Filestash");
    let data = PathBuf::from(std::env::var("LOCALAPPDATA").map_err(|_| "no %LOCALAPPDATA%")?)
        .join("Filestash");
    std::fs::create_dir_all(&data)?;

    match setup(
        args,
        fdrive_core::config::recall(&data).map(Credentials::from),
        root,
        data,
    ) {
        Ok(setup) => Ok(Some(setup)),
        Err(err) => {
            gui::alert(&err);
            std::process::exit(2);
        }
    }
}

fn setup(
    args: Args,
    stored: Option<Credentials>,
    root: PathBuf,
    data: PathBuf,
) -> Result<Setup, String> {
    if args.token.is_some() && args.user.is_some() {
        return Err("--token and --user cannot be combined".into());
    }
    if args.server.is_none() && (args.token.is_some() || args.user.is_some()) {
        return Err("--token and --user need --server".into());
    }
    if args.user.is_some() && args.password.as_deref().unwrap_or("").is_empty() {
        return Err("--user needs --password (or FILESTASH_PASSWORD)".into());
    }
    let server = args.server.as_deref().map(normalize_server);
    let credentials = match (&server, args.token, &args.user) {
        (Some(url), Some(token), _) => Some(Credentials {
            url: url.clone(),
            token,
            insecure: args.insecure,
            ..Default::default()
        }),
        (Some(url), None, Some(user)) => Some(Credentials {
            url: url.clone(),
            user: user.clone(),
            password: args.password.unwrap_or_default(),
            storage: args.storage.unwrap_or_default(),
            insecure: args.insecure,
            ..Default::default()
        }),
        (Some(_), None, None) => None,
        (None, ..) => stored,
    };
    let config_path = args.config.unwrap_or_else(|| data.join("fdrive.toml"));
    Ok(Setup {
        root,
        data,
        config_path,
        unregister: args.unregister,
        prompt_login: server.is_some() && credentials.is_none(),
        prefill_url: server,
        credentials,
    })
}
