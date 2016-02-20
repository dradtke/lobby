extern crate lobby;

use lobby::{Lobby, ScanResult};
use std::env::args;
use std::io::{self, BufReader, BufRead, Write};
use std::net::TcpStream;
use std::thread;

const ADDR: &'static str = "127.0.0.1:8080";

/// Run the server.
fn server_main() {
    let lobby = match Lobby::new(ADDR) {
        Ok(lobby) => lobby,
        Err(e) => { println!("{}", e); return; },
    };

    println!("Chatroom started, waiting on client connections.");

    loop {
        // Scan connected clients for input or updates.
        lobby.scan(|id, result| match result {
            ScanResult::Connected => println!("{} has connected.", lobby.name(id).unwrap()),
            ScanResult::Data(data) => {
                let msg = String::from_utf8(data).unwrap();
                let name = lobby.name(id).unwrap();
                lobby.message_rest(id, format!("{}: {}\n", name, msg).as_bytes());
            },
            ScanResult::IoError(err) => println!("io error: {}", err),
            ScanResult::Disconnected => println!("{} has disconnected.", lobby.name(id).unwrap()),
        });
    }
}

/// Connect to the server.
fn client_main() {
    println!("Connecting...");
    let mut stdout = io::stdout();

    match TcpStream::connect(ADDR) {
        Ok(mut stream) => {
            stdout.write("Name: ".as_bytes()).unwrap();
            stdout.flush().unwrap();

            let mut lines = BufReader::new(io::stdin()).lines();

            match lines.next() {
                Some(line) => if let Ok(line) = line {
                    stream.write_all(line.trim_right().as_bytes()).unwrap();
                    stream.write(&[0]).unwrap();
                    stream.flush().unwrap();
                },
                None => return,
            }

            {
                // Read messages from other clients and print them out.
                let stream = stream.try_clone().unwrap();
                thread::spawn(move || {
                    for line in BufReader::new(stream).lines() {
                        println!("{}", line.unwrap());
                    }
                });
            }

            for line in lines {
                if let Ok(line) = line {
                    if let Err(e) = stream.write_all(line.trim_right().as_bytes()) {
                        println!("{}", e);
                        break;
                    }
                }
            }
        },
        Err(e) => println!("{}", e),
    }
}

fn main() {
    match args().skip(1).next() {
        None => client_main(),
        Some(ref arg) if arg == "--serve" => server_main(),
        Some(arg) => panic!("unexpected argument: {}", arg),
    }
}
