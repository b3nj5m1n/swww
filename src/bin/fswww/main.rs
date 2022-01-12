use fork;
use image;
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
use structopt::StructOpt;

#[derive(Debug)]
enum Filter {
    Nearest,
    Triangle,
    CatmullRom,
    Gaussian,
    Lanczos3,
}

impl std::str::FromStr for Filter {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Nearest" => Ok(Self::Nearest),
            "Triangle" => Ok(Self::Triangle),
            "CatmullRom" => Ok(Self::CatmullRom),
            "Gaussian" => Ok(Self::Gaussian),
            "Lanczos3" => Ok(Self::Lanczos3),
            _ => Err("Non existing filter".to_string()),
        }
    }
}

impl std::fmt::Display for Filter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Nearest => write!(f, "Nearest"),
            Self::Triangle => write!(f, "Triangle"),
            Self::CatmullRom => write!(f, "CatmullRom"),
            Self::Gaussian => write!(f, "Gaussian"),
            Self::Lanczos3 => write!(f, "Lanczos3"),
        }
    }
}

#[derive(Debug, StructOpt)]
#[structopt(name = "fswww")]
///The Final Solution to your Wayland Wallpaper Woes
///
///Change what your monitors display as a background by controlling the fswww daemon at runtime.
///Supports animated gifs and putting different stuff in different monitors. I also did my best to
///make it as resource efficient as possible.
enum Fswww {
    /// Send an image (or animated gif) for the daemon to display
    Img {
        /// Path to the image to display
        #[structopt(parse(from_os_str))]
        file: PathBuf,

        /// Comma separated list of outputs to display the image at. If it isn't set, the image is
        /// displayed on all outputs
        #[structopt(short, long)]
        outputs: Option<String>,

        ///Filter to use when scaling images (run fswww img --help to see options).
        ///
        ///Available options are:
        ///
        ///Nearest | Triangle | CatmullRom | Gaussian | Lanczos3
        ///
        ///These are offered by the image crate (https://crates.io/crates/image). 'Nearest' is
        ///what I recommend for pixel art stuff, and ONLY for pixel art stuff. It is also the
        ///fastest filter.
        ///
        ///For non pixel art stuff, I would usually recommend one of the last three, though some
        ///experimentation will be necessary to see which one you like best. Also note they are
        ///all slower than Nearest. For some examples, see
        ///https://docs.rs/image/0.23.14/image/imageops/enum.FilterType.html.
        #[structopt(short, long, default_value = "Lanczos3")]
        filter: Filter,
    },

    ///Initialize the daemon. Exits if there is already a daemon running.
    Init {
        ///Don't fork the daemon. This will keep it running in the current terminal.
        ///
        ///The only advantage of this would be seeing the logging real time. Even then, for release
        ///builds we only log warnings and errors, so you won't be seeing much (ideally).
        #[structopt(long)]
        no_daemon: bool,
    },

    ///Kills the daemon
    Kill,

    ///Asks the daemon to print output information (names and dimensions). You may use this to find
    ///out valid values for the <fswww-img --outputs> option. If you want more detailed information
    ///about your outputs, I would recommed trying wlr-randr.
    Query,
}

fn spawn_daemon(no_daemon: bool) -> Result<(), String> {
    let mut cmd = Command::new("fswww-daemon");
    let spawn_err =
        "Failed to initialize fswww-daemon. Are you sure it is installed (and in the PATH)?";
    if no_daemon {
        if cmd.output().is_err() {
            return Err(spawn_err.to_string());
        };
    }
    match fork::fork() {
        Ok(fork::Fork::Child) => match fork::daemon(false, false) {
            Ok(fork::Fork::Child) => {
                cmd.output().expect(spawn_err);
                Ok(())
            }
            Ok(fork::Fork::Parent(_)) => Ok(()),
            Err(_) => Err("Couldn't daemonize forked process!".to_string()),
        },
        Ok(fork::Fork::Parent(_)) => Ok(()),
        Err(_) => Err("Couldn't create child process.".to_string()),
    }
}

fn main() -> Result<(), String> {
    let opts = Fswww::from_args();
    match opts {
        Fswww::Init { no_daemon } => {
            if get_socket().is_err() {
                spawn_daemon(no_daemon)?;
            } else {
                return Err("There seems to already be another instance running...".to_string());
            }
            if no_daemon {
                return Ok(()); //in this case, when the daemon stops we are done
            } else {
                send_request("__INIT__")?; //otherwise, we wait for the daemon's response
            }
        }
        Fswww::Kill => {
            kill()?;
            wait_for_response()?;
            let socket_path = get_socket_path();
            if let Err(e) = std::fs::remove_file(socket_path) {
                return Err(format!("{}", e));
            } else {
                println!("Stopped daemon and removed socket.");
                return Ok(());
            }
        }
        Fswww::Img {
            file,
            outputs,
            filter,
        } => send_img(file, outputs.unwrap_or("".to_string()), filter)?,
        Fswww::Query => send_request("__QUERY__")?,
    }

    wait_for_response()
}

fn send_img(path: PathBuf, outputs: String, filter: Filter) -> Result<(), String> {
    if let Err(e) = image::open(&path) {
        return Err(format!("Cannot open img {:?}: {}", path, e));
    }
    let abs_path = match path.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            return Err(format!("Failed to find absolute path: {}", e));
        }
    };
    let img_path_str = abs_path.to_str().unwrap();
    let msg = format!("__IMG__\n{}\n{}\n{}\n", filter, outputs, img_path_str);
    send_request(&msg)?;

    Ok(())
}

fn kill() -> Result<(), String> {
    let mut socket = get_socket()?;
    match socket.write(b"__KILL__") {
        Ok(_) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

fn wait_for_response() -> Result<(), String> {
    let mut socket = get_socket()?;
    let mut buf = String::with_capacity(100);
    let mut error = String::new();

    for _ in 0..20 {
        match socket.read_to_string(&mut buf) {
            Ok(_) => {
                if buf.starts_with("Ok\n") {
                    if buf.len() > 3 {
                        print!("{}", &buf[3..]);
                    }
                    return Ok(());
                } else if buf.starts_with("Err\n") {
                    return Err(format!("daemon sent back: {}", &buf[4..]));
                } else {
                    return Err(format!("daemon returned a badly formatted answer: {}", buf));
                }
            }
            Err(e) => error = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err("Error while waiting for response: ".to_string() + &error)
}

fn send_request(request: &str) -> Result<(), String> {
    let mut socket = get_socket()?;
    let mut error = String::new();

    for _ in 0..5 {
        match socket.write_all(request.as_bytes()) {
            Ok(_) => return Ok(()),
            Err(e) => error = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("Failed to send request: ".to_string() + &error)
}

///Always sets connection to nonblocking. This is because the daemon will always listen to
///connections in a nonblocking fashion, so it makes sense to make this the standard for the whole
///program. The only difference is that we will have to make timeouts manually by trying to connect
///several times in a row, waiting some time between every attempt.
fn get_socket() -> Result<UnixStream, String> {
    let path = get_socket_path();
    let mut error = String::new();
    //We try to connect 5 fives, waiting 100 milis in between
    for _ in 0..5 {
        match UnixStream::connect(&path) {
            Ok(socket) => {
                if let Err(e) = socket.set_nonblocking(true) {
                    return Err(format!(
                        "Failed to set nonblocking connection: {}",
                        e.to_string()
                    ));
                }
                return Ok(socket);
            }
            Err(e) => error = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("Failed to connect to socket: ".to_string() + &error)
}

fn get_socket_path() -> PathBuf {
    let runtime_dir = if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        dir
    } else {
        "/tmp/fswww".to_string()
    };
    let runtime_dir = Path::new(&runtime_dir);
    runtime_dir.join("fswww.socket")
}
