use ignore::WalkBuilder;
use ssh2::Session;
use std::fs;
use std::io::Write;
use std::io::{self, Read};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct AuthKey {
    pub pubkey: Option<PathBuf>,
    pub privekey: PathBuf,
    pub passphrase: Option<String>,
}

pub enum Auth {
    Password(String),
    AuthKey(AuthKey),
    Agent,
}

pub struct Config {
    pub server_addr: String,
    pub username: String,
    pub auth: Auth,
    pub command: String,
}

pub fn exec(conf: Config) -> Result<(), Box<dyn std::error::Error>> {
    let tcp = TcpStream::connect(conf.server_addr)?;
    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.handshake()?;

    match conf.auth {
        Auth::Password(p) => {
            sess.userauth_password(conf.username.as_str(), p.as_str())?;
        }
        Auth::AuthKey(auth_key) => {
            sess.userauth_pubkey_file(
                conf.username.as_str(),
                auth_key.pubkey.as_ref().map(|p| p.as_path()),
                auth_key.privekey.as_path(),
                auth_key.passphrase.as_ref().map(|p| p.as_str()),
            )?;
        }
        Auth::Agent => {
            let mut agent = sess.agent()?;
            agent.connect()?;
            agent.list_identities()?;
            let identities = agent.identities()?;
            if identities.len() == 0 {
                return Err("No identities found in the ssh-agent".into());
            }
            sess.userauth_agent(conf.username.as_str())?;
        }
    }

    let sftp = sess.sftp()?;

    let local_dir = "./";
    // get current timestep as file name. e.g. ~/.cserun/temp/2024-02-14-01-10-40-224/
    let temp_dir_name = chrono::Local::now()
        .format("%Y-%m-%d-%H-%M-%S-%3f")
        .to_string();
    let remote_dir = format!(".cserun/temp/{}", temp_dir_name); // ssh2's sftp use ~/ as root, no need to add ~/
    let remote_dir_path = Path::new(&remote_dir);
    println!("remote_dir: {}", remote_dir);

    // create the remote dir
    sftp_mkdir_recursive(&sftp, remote_dir_path)?;
    println!("Created remote dir: {:?}", remote_dir_path);

    // log the command to command.txt
    let mut remote_command_file = sftp.create(remote_dir_path.join("command.txt").as_path())?;
    remote_command_file.write_all(conf.command.as_bytes())?;
    println!("Uploaded command.txt");

    // setup the container dir
    let container_path = remote_dir_path.join("container");
    upload_dir(&sftp, Path::new(local_dir), container_path.as_path())?;

    let mut channel = sess.channel_session()?;
    // before exec, try to cd to the remote dir, if failed, exit
    let command = format!("cd {}/container && {}", remote_dir, conf.command);
    channel.exec(&command)?;

    // set to unblocking mode
    sess.set_blocking(false);

    let mut buffer = [0; 4096];
    loop {
        if channel.eof() {
            // if channel closed, break the loop
            break;
        }

        let mut is_data_available = false;

        // try to read the standard output
        match channel.read(&mut buffer) {
            Ok(size) if size > 0 => {
                print!("{}", String::from_utf8_lossy(&buffer[..size]));
                is_data_available = true;
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }

        // try to read the standard error
        match channel.stderr().read(&mut buffer) {
            Ok(size) if size > 0 => {
                eprint!("{}", String::from_utf8_lossy(&buffer[..size]));
                is_data_available = true;
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }

        if !is_data_available {
            // wait for 100ms to reduce CPU usage
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    channel.wait_close()?;
    println!("\nExit status: {}", channel.exit_status()?);

    Ok(())
}

fn sftp_mkdir_recursive(sftp: &ssh2::Sftp, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut current_path = PathBuf::new();
    for component in path.components() {
        current_path.push(component);
        if let Ok(metadata) = sftp.stat(current_path.as_path()) {
            if metadata.is_dir() {
                continue;
            }
            return Err(format!("{:?} is not a directory", current_path).into());
        }
        sftp.mkdir(current_path.as_path(), 0o755)?;
    }
    Ok(())
}

// upload every file and directory in the local directory to remote directory
fn upload_dir(
    sftp: &ssh2::Sftp,
    local_path: &Path,
    remote_base_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let walker = WalkBuilder::new(local_path)
        .ignore(true) // https://docs.rs/ignore/latest/ignore/struct.WalkBuilder.html#method.ignore
        .git_ignore(true) // https://docs.rs/ignore/latest/ignore/struct.WalkBuilder.html#method.git_ignore
        .build();

    for result in walker {
        if let Ok(entry) = result {
            let path = entry.path();
            // Calculate the relative path
            if let Ok(strip_path) = path.strip_prefix(local_path) {
                let remote_path = remote_base_path.join(strip_path);
                if path.is_dir() {
                    // Make sure the remote directory exists
                    match sftp.mkdir(&remote_path, 0o755) {
                        Ok(_) => println!("Created directory: {:?}", remote_path),
                        Err(err) => {
                            println!("Directory creation error (might already exist): {:?}", err)
                        }
                    }
                } else {
                    upload_file(sftp, path, &remote_path)?;
                }
            }
        }
    }

    Ok(())
}

fn upload_file(
    sftp: &ssh2::Sftp,
    local_path: &Path,
    remote_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = fs::File::open(local_path)?;
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;

    let mut remote_file = sftp.create(remote_path)?;
    remote_file.write_all(&contents)?;
    println!("Uploaded file: {:?}", remote_path);

    Ok(())
}
