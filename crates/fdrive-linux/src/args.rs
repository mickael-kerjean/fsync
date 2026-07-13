use std::io::{BufRead, Write};
use std::path::PathBuf;

use clap::Parser;
use fdrive_linux::gui::{self, Credentials};

#[derive(Parser)]
#[command(name = "fdrive", about = "Filestash drive client")]
struct Args {
    #[arg(value_name = "MOUNT")]
    mount: PathBuf,
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
    data: Option<PathBuf>,
    #[arg(long)]
    insecure: bool,
}

pub struct Setup {
    pub mount: PathBuf,
    pub data: PathBuf,
    pub prefill: Credentials,
    pub credentials: Option<Credentials>,
    pub stored: bool,
    pub prompt_login: bool,
}

pub fn init() -> Result<Setup, Box<dyn std::error::Error>> {
    let mut args = Args::parse();
    if args.user.is_some() && args.password.as_deref().unwrap_or("").is_empty() {
        args.password = Some(prompt_password()?);
    }
    let data = args.data.clone().unwrap_or_else(gui::default_data);
    let setup = setup(
        args,
        fdrive_core::config::recall(&data).map(Credentials::from),
    )?;
    std::fs::create_dir_all(&setup.data)?;
    Ok(setup)
}

fn setup(args: Args, stored: Option<Credentials>) -> Result<Setup, String> {
    if args.token.is_some() && args.user.is_some() {
        return Err("--token and --user cannot be combined".into());
    }
    if args.server.is_none() && (args.token.is_some() || args.user.is_some()) {
        return Err("--token and --user need --server".into());
    }
    let server = args.server.as_deref().map(gui::normalize_server);
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
    let stored = server.is_none() && credentials.is_some();
    Ok(Setup {
        mount: args.mount,
        data: args.data.unwrap_or_else(gui::default_data),
        prompt_login: server.is_some() && credentials.is_none(),
        prefill: Credentials {
            url: server.unwrap_or_default(),
            insecure: args.insecure,
            ..Default::default()
        },
        credentials,
        stored,
    })
}

fn prompt_password() -> std::io::Result<String> {
    use std::os::fd::AsRawFd;
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")?;
    write!(tty, "Password: ")?;
    tty.flush()?;
    let fd = tty.as_raw_fd();
    let mut term = unsafe { std::mem::zeroed::<libc::termios>() };
    let echo_off = unsafe { libc::tcgetattr(fd, &mut term) } == 0;
    let saved = term;
    if echo_off {
        term.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) };
    }
    let mut line = String::new();
    let read = std::io::BufReader::new(&tty).read_line(&mut line);
    if echo_off {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
        let _ = writeln!(tty);
    }
    read?;
    Ok(line.trim_end_matches('\n').to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Args {
        Args::try_parse_from([&["fdrive", "/tmp/mnt"], argv].concat()).unwrap()
    }

    #[test]
    fn token_mode() {
        let s = setup(
            parse(&["--server", "localhost:8334", "--token", "t0k"]),
            None,
        )
        .unwrap();
        let creds = s.credentials.unwrap();
        assert_eq!(creds.url, "https://localhost:8334");
        assert_eq!(creds.token, "t0k");
        assert!(!s.prompt_login);
    }

    #[test]
    fn password_mode() {
        let mut args = parse(&[
            "--server",
            "http://x/",
            "--user",
            "joe",
            "--storage",
            "docs",
        ]);
        args.password = Some("s3cret".into());
        let creds = setup(args, None).unwrap().credentials.unwrap();
        assert_eq!(creds.url, "http://x");
        assert_eq!(creds.user, "joe");
        assert_eq!(creds.password, "s3cret");
        assert_eq!(creds.storage, "docs");
        assert!(creds.token.is_empty());
    }

    #[test]
    fn server_alone_prompts_login() {
        let s = setup(parse(&["--server", "http://x"]), None).unwrap();
        assert!(s.credentials.is_none());
        assert!(s.prompt_login);
        assert_eq!(s.prefill.url, "http://x");
    }

    #[test]
    fn no_args_recalls_stored_session() {
        let stored = Credentials {
            url: "http://x".into(),
            token: "t0k".into(),
            ..Default::default()
        };
        let s = setup(parse(&[]), Some(stored)).unwrap();
        assert!(s.stored);
        assert_eq!(s.credentials.unwrap().token, "t0k");
        assert_eq!(s.mount, PathBuf::from("/tmp/mnt"));
    }

    #[test]
    fn explicit_server_ignores_stored_session() {
        let stored = Credentials {
            url: "http://old".into(),
            token: "t0k".into(),
            ..Default::default()
        };
        let s = setup(parse(&["--server", "http://new"]), Some(stored)).unwrap();
        assert!(s.credentials.is_none());
        assert!(s.prompt_login);
    }

    #[test]
    fn conflicting_and_orphan_flags_are_rejected() {
        let mut args = parse(&["--server", "http://x", "--user", "joe"]);
        args.token = Some("t0k".into());
        assert!(setup(args, None).is_err());
        assert!(setup(parse(&["--user", "joe"]), None).is_err());
        assert!(Args::try_parse_from(["fdrive"]).is_err());
    }
}
