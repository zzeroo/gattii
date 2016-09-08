extern crate serial;

use core::num;
use std::fs::File;
use std::io;
use std::io::prelude::*;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::mpsc::{channel, Sender, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use self::serial::{BaudRate, PortInfo};
use self::serial::prelude::*;

pub enum SerialCommand {
    ConnectToPort { name: String, baud: usize },
    ChangeBaud(usize),
    ChangePort(String),
    Disconnect,
    SendData(Vec<u8>),
    SendFile(PathBuf),
    CancelSendFile,
}

pub enum SerialResponse {
    Data(Vec<u8>),
    SendingFileCanceled,
    SendingFileComplete,
    SendingFileError(String),
    OpenPortSuccess,
    OpenPortError(String),
    DisconnectSuccess,
}

pub enum GeneralError {
    Io(io::Error),
    Parse(num::ParseIntError),
    Send(mpsc::SendError<SerialCommand>),
}

pub struct SerialThread {
    pub from_port_chan_rx: Receiver<SerialResponse>,
    pub to_port_chan_tx: Sender<SerialCommand>
}

pub fn list_ports() -> serial::Result<Vec<PortInfo>> {
    serial::list_ports()
}

impl SerialThread {
    pub fn new<F: Fn() + Send + 'static>(callback: F) -> Self {

        let (from_port_chan_tx, from_port_chan_rx) = channel();
        let (to_port_chan_tx, to_port_chan_rx) = channel();

        // Open a thread to monitor the active serial channel. This thread is always-running and listening
        // for various port-related commands, but is not necessarily always connected to the port.
        thread::spawn(move || {
            let mut port: Option<Box<serial::SystemPort>> = None;
            let mut port_settings: serial::PortSettings = serial::PortSettings {
                baud_rate: serial::Baud115200,
                char_size: serial::Bits8,
                parity: serial::ParityNone,
                stop_bits: serial::Stop1,
                flow_control: serial::FlowNone,
            };
            let mut read_file: Option<Box<File>> = None;

            let mut serial_buf: Vec<u8> = vec![0; 1000];
            let mut serial_buf_rx = [0; 1000];
            let mut last_send_time = Instant::now();
            loop {
                // First check if we have any incoming commands
                match to_port_chan_rx.try_recv() {
                    Ok(SerialCommand::ConnectToPort { name, baud }) => {
                        println!("Connecting to {} at {}", &name, baud);
                        if let Ok(mut p) = open_port(name.clone(), baud) {
                            // Set the timeout to 1ms to keep a tight event loop
                            p.set_timeout(Duration::from_millis(1));
                            port = Some(p);
                            from_port_chan_tx.send(SerialResponse::OpenPortSuccess).unwrap();
                        } else {
                            from_port_chan_tx.send(SerialResponse::OpenPortError(String::from(format!("Failed to open port '{}'", &name)))).unwrap();
                        }
                        callback();
                    }
                    Ok(SerialCommand::ChangeBaud(baud)) => {
                        if let Some(ref mut p) = port {
                            println!("Changing baud to {}", baud);
                            let baud_rate = BaudRate::from_speed(baud);
                            p.reconfigure(&|s| {
                                    s.set_baud_rate(baud_rate).unwrap();
                                    Ok(())
                                })
                                .unwrap();
                            port_settings.set_baud_rate(baud_rate).unwrap();
                            println!("{:?}", port_settings);
                        }
                    }
                    Ok(SerialCommand::ChangePort(name)) => {
                        println!("Changing port to {}", name);
                        let mut p = Box::new(serial::open(&name).unwrap());
                        p.configure(&port_settings).unwrap();
                        port = Some(p);
                    }
                    Ok(SerialCommand::Disconnect) => {
                        println!("Disconnecting");
                        port = None;
                        read_file = None;
                        from_port_chan_tx.send(SerialResponse::DisconnectSuccess).unwrap();
                        callback();
                    }
                    Ok(SerialCommand::SendData(d)) => {
                        if let Some(ref mut p) = port {
                            match p.write(d.as_ref()) {
                                Ok(_) => (),
                                Err(e) => println!("Error in SendData: {:?}", e),
                            }
                        }
                    }
                    Ok(SerialCommand::SendFile(f)) => {
                        if let Some(_) = port {
                            println!("Sending file {:?}", f);
                            if let Ok(new_file) = File::open(f) {
                                read_file = Some(Box::new(new_file));
                            }
                        } else {
                            from_port_chan_tx.send(SerialResponse::SendingFileError(String::from("No open port to send file!"))).unwrap();
                            callback();
                        }
                    }
                    Ok(SerialCommand::CancelSendFile) => {
                        read_file = None;
                        from_port_chan_tx.send(SerialResponse::SendingFileCanceled).unwrap();
                        callback();
                    }
                    Err(TryRecvError::Empty) |
                    Err(TryRecvError::Disconnected) => (),
                }

                if let Some(ref mut p) = port {
                    let rx_data_len = match p.read(serial_buf.as_mut_slice()) {
                        Ok(t) => t,
                        Err(_) => 0,
                    };
                    if rx_data_len > 0 {
                        let send_data = SerialResponse::Data(serial_buf[..rx_data_len].to_vec());
                        from_port_chan_tx.send(send_data).unwrap();
                        callback();
                    }

                    // If a file has been opened, read the next 1ms of data from it as
                    // determined by the current baud rate.
                    let mut read_len : Option<usize> = None;
                    if let Some(ref mut file) = read_file {
                        let mut byte_as_serial_bits = 1 + 8;
                        if port_settings.parity != serial::ParityNone {
                            byte_as_serial_bits += 1;
                        }
                        if port_settings.stop_bits == serial::Stop1 {
                            byte_as_serial_bits += 1;
                        } else if port_settings.stop_bits == serial::Stop2 {
                            byte_as_serial_bits += 2;
                        }
                        // Write 10ms of data at a time to account for loop time
                        // variation
                        let data_packet_time = 10; // ms
                        if last_send_time.elapsed().subsec_nanos() > data_packet_time * 1_000_000 {
                            let tx_data_len = port_settings.baud_rate.speed() / byte_as_serial_bits / (1000 / data_packet_time as usize);
                            println!("Reading {} bytes", tx_data_len);
                            if let Ok(len) = file.read(&mut serial_buf_rx[..tx_data_len]) {
                                read_len = Some(len);
                            } else {
                                println!("Failed to read {} bytes", tx_data_len);
                            }
                        } else {
                            read_len = Some(0);
                        }
                    }

                    match read_len {
                        Some(x) => if x > 0 {
                            if let Err(_) = p.write(&serial_buf_rx[..x]) {
                                println!("Failed to send {} bytes", x);
                                read_file = None;
                            }
                            last_send_time = Instant::now();
                        },
                        None => if read_file.is_some() {
                            read_file = None;
                            from_port_chan_tx.send(SerialResponse::SendingFileComplete).unwrap();
                            callback();
                        }
                    }
                }
            }
        });

        SerialThread {
            from_port_chan_rx: from_port_chan_rx,
            to_port_chan_tx: to_port_chan_tx
        }
    }

    pub fn send_port_open_cmd(&self, port_name: String, baud_rate: String) -> Result<(), GeneralError> {
        let baud_rate: usize = try!(baud_rate.parse().map_err(GeneralError::Parse));
        let tx = &self.to_port_chan_tx;
        try!(tx.send(SerialCommand::ConnectToPort { name: port_name, baud: baud_rate }).map_err(GeneralError::Send)); // TODO: Remove in favor of impl From for GeneralError
        Ok(())
    }

    pub fn send_port_close_cmd(&self) -> Result<(), GeneralError> {
        let tx = &self.to_port_chan_tx;
        try!(tx.send(SerialCommand::Disconnect).map_err(GeneralError::Send)); // TODO: Remove in favor of impl From for GeneralError
        Ok(())
    }

    pub fn send_port_change_baud_cmd(&self, baud_rate: String) -> Result<(), GeneralError> {
        let baud_rate: usize = try!(baud_rate.parse().map_err(GeneralError::Parse));
        let tx = &self.to_port_chan_tx;
        try!(tx.send(SerialCommand::ChangeBaud(baud_rate)).map_err(GeneralError::Send)); // TODO: Remove in favor of impl From for GeneralError
        Ok(())
    }

    pub fn send_port_change_port_cmd(&self, port_name: String) -> Result<(), GeneralError> {
        let tx = &self.to_port_chan_tx;
        try!(tx.send(SerialCommand::ChangePort(port_name)).map_err(GeneralError::Send)); // TODO: Remove in favor of impl From for GeneralError
        Ok(())
    }

    pub fn send_port_data_cmd(&self, data: Vec<u8>) -> Result<(), GeneralError> {
        let tx = &self.to_port_chan_tx;
        try!(tx.send(SerialCommand::SendData(data)).map_err(GeneralError::Send)); // TODO: Remove in favor of impl From for GeneralError
        Ok(())
    }

    pub fn send_port_file_cmd(&self, path: PathBuf) -> Result<(), GeneralError> {
        let tx = &self.to_port_chan_tx;
        try!(tx.send(SerialCommand::SendFile(path)).map_err(GeneralError::Send)); // TODO: Remove in favor of impl From for GeneralError
        Ok(())
    }

    pub fn send_cancel_file_cmd(&self) -> Result<(), GeneralError> {
        let tx = &self.to_port_chan_tx;
        try!(tx.send(SerialCommand::CancelSendFile).map_err(GeneralError::Send)); // TODO: Remove in favor of impl From for GeneralError
        Ok(())
    }
}

fn open_port(port_name: String, baud_rate: usize) -> serial::Result<Box<serial::SystemPort>> {
    // Open the specified serial port
    let mut port = try!(serial::open(&port_name));

    // Configure the port settings
    try!(port.reconfigure(&|settings| {
        try!(settings.set_baud_rate(BaudRate::from_speed(baud_rate)));
        settings.set_char_size(serial::Bits8);
        settings.set_parity(serial::ParityNone);
        settings.set_stop_bits(serial::Stop1);
        settings.set_flow_control(serial::FlowNone);
        Ok(())
    }));

    Ok(Box::new(port))
}