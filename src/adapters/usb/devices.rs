use libusb;
use std::{process::exit, slice::Iter, str::FromStr};

use super::{UsbAdapter, UsbTimeout};
use adapters::AdapterError;
use serial::SerialNumber;

/// USB vendor ID for Yubico
pub const YUBICO_VENDOR_ID: u16 = 0x1050;

/// USB product ID for the YubiHSM2
pub const YUBIHSM2_PRODUCT_ID: u16 = 0x0030;

lazy_static! {
    /// Global USB context for accessing YubiHSM2s
    static ref GLOBAL_USB_CONTEXT: libusb::Context = libusb::Context::new().unwrap_or_else(|e| {
        eprintln!("*** ERROR: yubihsm-rs USB context init failed: {}", e);
        exit(1);
    });
}

/// The `UsbDevices` type enumerates available YubiHSM2 devices by their serial
/// number and opening connections to them (in the form of a `UsbAdapter`).
pub struct UsbDevices(Vec<HsmDevice>);

impl UsbDevices {
    /// Return the serial numbers of all connected YubiHSM2s
    pub fn serials() -> Result<Vec<SerialNumber>, AdapterError> {
        let devices = Self::new(UsbTimeout::default())?;
        let serials: Vec<_> = devices.iter().map(|a| a.serial_number).collect();
        Ok(serials)
    }

    /// Open a YubiHSM2, either selecting one with a particular serial number
    /// or opening the only available one if `None`there is only one connected
    pub fn open(
        serial_number: Option<SerialNumber>,
        timeout: UsbTimeout,
    ) -> Result<UsbAdapter, AdapterError> {
        let mut devices = Self::new(timeout)?;

        if let Some(sn) = serial_number {
            while let Some(device) = devices.0.pop() {
                if device.serial_number == sn {
                    return device.open(timeout);
                }
            }

            adapter_fail!(
                UsbError,
                "no YubiHSM 2 found with serial number: {:?}",
                serial_number
            )
        } else if devices.0.len() == 1 {
            devices.0.remove(0).open(timeout)
        } else {
            adapter_fail!(
                UsbError,
                "expected a single YubiHSM device to be connected, found {}: {:?}",
                devices.0.len(),
                devices
                    .0
                    .iter()
                    .map(|d| d.serial_number.as_str().to_owned())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    /// Detect connected YubiHSM 2s, returning a collection of them
    pub fn new(timeout: UsbTimeout) -> Result<Self, AdapterError> {
        let device_list = GLOBAL_USB_CONTEXT.devices()?;
        let mut devices = vec![];

        println!("USB: enumerating devices...");

        for device in device_list.iter() {
            let desc = device.device_descriptor()?;

            if desc.vendor_id() != YUBICO_VENDOR_ID || desc.product_id() != YUBIHSM2_PRODUCT_ID {
                continue;
            }

            println!(
                "USB(bus={},addr={}): found YubiHSM 2 device",
                device.bus_number(),
                device.address(),
            );

            let mut handle = device.open().map_err(|e| {
                adapter_err!(
                    UsbError,
                    "USB(bus={},addr={}): error opening device: {}",
                    device.bus_number(),
                    device.address(),
                    e
                )
            })?;

            handle.reset().map_err(|error| match error {
                libusb::Error::NoDevice => adapter_err!(
                    DeviceBusyError,
                    "USB(bus={},addr={}): couldn't reset device (already in use or disconnected)",
                    device.bus_number(),
                    device.address()
                ),
                other => adapter_err!(
                    UsbError,
                    "USB(bus={},addr={}): error resetting device: {}",
                    device.bus_number(),
                    device.address(),
                    other
                ),
            })?;

            let language = *handle
                .read_languages(timeout.duration())?
                .first()
                .ok_or_else(|| {
                    adapter_err!(
                        UsbError,
                        "USB(bus={},addr={}): couldn't read YubiHSM serial number (missing language info)",
                        device.bus_number(),
                        device.address(),
                    )
                })?;

            let serial_number = SerialNumber::from_str(&handle.read_serial_number_string(
                language,
                &desc,
                timeout.duration(),
            )?)?;

            println!(
                "USB(bus={},addr={}): successfully opened YubiHSM 2 device (serial #{})",
                device.bus_number(),
                device.address(),
                serial_number.as_str(),
            );

            devices.push(HsmDevice {
                serial_number,
                device,
            });
        }

        if devices.is_empty() {
            println!("no YubiHSM 2 devices found");
        }

        Ok(UsbDevices(devices))
    }

    /// Iterate over the detected YubiHSM 2s
    pub fn iter(&self) -> Iter<HsmDevice> {
        self.0.iter()
    }
}

/// A device which has been detected to be a YubiHSM2
pub struct HsmDevice {
    /// Serial number of the device
    pub serial_number: SerialNumber,

    /// Underlying `libusb` device
    pub(super) device: libusb::Device<'static>,
}

impl HsmDevice {
    /// Open this device, consuming it and creating a `UsbAdapter`
    pub fn open(self, timeout: UsbTimeout) -> Result<UsbAdapter, AdapterError> {
        UsbAdapter::new(&self.device, self.serial_number, timeout)
    }
}
