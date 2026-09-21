//! LAN discovery: iroh's mDNS address lookup under the `tcode` service name.
//! The machine name rides along as mDNS user data so nearby machines can be
//! listed by name; it is never published anywhere else.
use iroh::{
    EndpointId,
    address_lookup::{AddressLookup, EndpointData, Error, Item, UserData},
};
use iroh_mdns_address_lookup::MdnsAddressLookup;

pub(crate) const SERVICE_NAME: &str = "tcode";

#[derive(Debug, Clone)]
pub(crate) struct LanLookup {
    pub(crate) inner: MdnsAddressLookup,
    name: Option<UserData>,
}

impl LanLookup {
    /// Must run inside the runtime; `advertise` is false for devices that
    /// only browse.
    pub(crate) fn new(
        endpoint_id: EndpointId,
        advertise: bool,
        name: Option<&str>,
    ) -> std::io::Result<Self> {
        let inner = MdnsAddressLookup::builder()
            .service_name(SERVICE_NAME)
            .advertise(advertise)
            .build(endpoint_id)
            .map_err(std::io::Error::other)?;
        let name = name.and_then(|name| {
            let mut name = name.trim().to_owned();
            while name.len() > UserData::MAX_LENGTH {
                name.pop();
            }
            name.parse().ok()
        });
        Ok(Self { inner, name })
    }
}

impl AddressLookup for LanLookup {
    fn publish(&self, data: &EndpointData) {
        let mut data = data.clone();
        data.set_user_data(self.name.clone());
        self.inner.publish(&data);
    }

    fn resolve(
        &self,
        endpoint_id: EndpointId,
    ) -> Option<futures_lite::stream::Boxed<Result<Item, Error>>> {
        self.inner.resolve(endpoint_id)
    }
}
