use libusb;
use std::{
    fmt::{self, Debug},
    process::exit,
    slice::Iter,
    str::FromStr,
    time::Duration,
    vec::IntoIter,
};

use super::{UsbConnection, UsbTimeout};
use super::{
    YUBICO_VENDOR_ID, YUBIHSM2_BULK_IN_ENDPOINT, YUBIHSM2_INTERFACE_NUM, YUBIHSM2_PRODUCT_ID,
};
use crate::command::MAX_MSG_SIZE;
use crate::connector::{
    ConnectionError,
    ConnectionErrorKind::{DeviceBusyError, UsbError},
};
use crate::serial_number::SerialNumber;

lazy_static! {
    /// Global USB context for accessing YubiHSM2s
    static ref GLOBAL_USB_CONTEXT: libusb::Context = libusb::Context::new().unwrap_or_else(|e| {
        eprintln!("*** ERROR: yubihsm-rs USB context init failed: {}", e);
        exit(1);
    });
}

/// A collection of detected YubiHSM 2 devices, represented as `Device`
pub struct Devices(Vec<Device>);

impl Devices {
    /// Return the serial numbers of all connected YubiHSM2s
    pub fn serial_numbers() -> Result<Vec<SerialNumber>, ConnectionError> {
        let devices = Self::detect(UsbTimeout::default())?;
        let serials: Vec<_> = devices.iter().map(|a| a.serial_number).collect();
        Ok(serials)
    }

    /// Open a YubiHSM2, either selecting one with a particular serial number
    /// or opening the only available one if `None`there is only one connected
    pub fn open(
        serial_number: Option<SerialNumber>,
        timeout: UsbTimeout,
    ) -> Result<UsbConnection, ConnectionError> {
        let mut devices = Self::detect(timeout)?;

        if let Some(sn) = serial_number {
            while let Some(device) = devices.0.pop() {
                if device.serial_number == sn {
                    return device.open(timeout);
                }
            }

            fail!(
                UsbError,
                "no YubiHSM2 found with serial number: {:?}",
                serial_number
            )
        } else {
            match devices.0.len() {
                1 => devices.0.remove(0).open(timeout),
                0 => fail!(UsbError, "no YubiHSM2 devices detected"),
                _ => fail!(
                    UsbError,
                    "expected a single YubiHSM2 device to be connected, found {}: {}",
                    devices.0.len(),
                    devices
                        .0
                        .iter()
                        .map(|d| d.serial_number.as_str().to_owned())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
    }

    /// Detect connected YubiHSM 2s, returning a collection of them
    pub fn detect(timeout: UsbTimeout) -> Result<Self, ConnectionError> {
        let device_list = GLOBAL_USB_CONTEXT.devices()?;
        let mut devices = vec![];

        debug!("USB: enumerating devices...");

        for device in device_list.iter() {
            let desc = device.device_descriptor()?;

            if desc.vendor_id() != YUBICO_VENDOR_ID || desc.product_id() != YUBIHSM2_PRODUCT_ID {
                continue;
            }

            usb_debug!(device, "found YubiHSM device");

            let mut handle = device
                .open()
                .map_err(|e| usb_err!(device, "error opening device: {}", e))?;

            handle.reset().map_err(|error| match error {
                libusb::Error::NoDevice => err!(
                    DeviceBusyError,
                    "USB(bus={},addr={}): couldn't reset device (already in use or disconnected)",
                    device.bus_number(),
                    device.address()
                ),
                other => usb_err!(device, "error resetting device: {}", other),
            })?;

            let language = *handle
                .read_languages(timeout.duration())?
                .first()
                .ok_or_else(|| {
                    usb_err!(
                        device,
                        "couldn't read YubiHSM serial number (missing language info)"
                    )
                })?;

            let t = timeout.duration();
            let manufacturer = handle.read_manufacturer_string(language, &desc, t)?;
            let product = handle.read_product_string(language, &desc, t)?;
            let serial_number = handle.read_serial_number_string(language, &desc, t)?;
            let product_name = format!("{} {}", manufacturer, product);

            debug!(
                "USB(bus={},addr={}): found {} (serial #{})",
                device.bus_number(),
                device.address(),
                &product_name,
                serial_number.as_str(),
            );

            devices.push(Device::new(
                device,
                product_name,
                SerialNumber::from_str(&serial_number)?,
            ));
        }

        if devices.is_empty() {
            debug!("no YubiHSM 2 devices found");
        }

        Ok(Devices(devices))
    }

    /// Number of detected devices
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Did we fail to find any YubiHSM2 devices?
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Borrow the detected devices as a slice
    pub fn as_slice(&self) -> &[Device] {
        self.0.as_slice()
    }

    /// Iterate over the detected YubiHSM 2s
    pub fn iter(&self) -> Iter<Device> {
        self.0.iter()
    }
}

impl IntoIterator for Devices {
    type Item = Device;
    type IntoIter = IntoIter<Device>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// A USB device we've identified as a YubiHSM2
pub struct Device {
    /// Underlying `libusb` device
    pub(super) device: libusb::Device<'static>,

    /// Product vendor and name
    pub product_name: String,

    /// Serial number of the YubiHSM2 device
    pub serial_number: SerialNumber,
}

impl Device {
    /// Create a new device
    pub(super) fn new(
        device: libusb::Device<'static>,
        product_name: String,
        serial_number: SerialNumber,
    ) -> Self {
        Self {
            serial_number,
            product_name,
            device,
        }
    }

    /// Open this device, consuming it and creating a `UsbConnection`
    pub fn open(self, timeout: UsbTimeout) -> Result<UsbConnection, ConnectionError> {
        let connection = UsbConnection::create(self, timeout)?;

        debug!(
            "USB(bus={},addr={}): successfully opened {} (serial #{})",
            connection.device().bus_number(),
            connection.device().address(),
            connection.device().product_name.as_str(),
            connection.device().serial_number.as_str(),
        );

        Ok(connection)
    }

    /// Get the bus number for this device
    pub fn bus_number(&self) -> u8 {
        self.device.bus_number()
    }

    /// Get the address for this device
    pub fn address(&self) -> u8 {
        self.device.address()
    }

    /// Open a handle to the underlying device (for use by `UsbConnection`)
    pub(super) fn open_handle(&self) -> Result<libusb::DeviceHandle<'static>, ConnectionError> {
        let mut handle = self.device.open()?;
        handle.reset()?;
        handle.claim_interface(YUBIHSM2_INTERFACE_NUM)?;

        // Flush any unconsumed messages still in the buffer
        flush(&mut handle)?;

        Ok(handle)
    }
}

impl Debug for Device {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "yubihsm::usb::Device(bus={} addr={} serial=#{})",
            self.bus_number(),
            self.address(),
            self.serial_number,
        )
    }
}

/// Flush any unconsumed messages still in the buffer to get the connection
/// back into a clean state
fn flush(handle: &mut libusb::DeviceHandle) -> Result<(), ConnectionError> {
    let mut buffer = [0u8; MAX_MSG_SIZE];

    // Use a near instantaneous (but non-zero) timeout to drain the buffer.
    // Zero is interpreted as wait forever.
    let timeout = Duration::from_millis(1);

    match handle.read_bulk(YUBIHSM2_BULK_IN_ENDPOINT, &mut buffer, timeout) {
        Ok(_) | Err(libusb::Error::Timeout) => Ok(()),
        Err(e) => Err(e.into()),
    }
}