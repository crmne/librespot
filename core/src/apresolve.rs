use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use hyper::{Method, Request};
use serde::{Deserialize, Serialize};

use crate::Error;

pub type SocketAddress = (String, u16);

/// How long a cached resolve stays usable. A stale entry only costs a
/// reconnect to a dead access point, which the retry loop covers; the age
/// bounds how far the list can drift from Spotify's current one.
const CACHE_MAX_AGE_SECS: u64 = 24 * 60 * 60;

#[derive(Default)]
pub struct AccessPoints {
    accesspoint: VecDeque<SocketAddress>,
    dealer: VecDeque<SocketAddress>,
    spclient: VecDeque<SocketAddress>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ApResolveData {
    accesspoint: Vec<String>,
    dealer: Vec<String>,
    spclient: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct CachedResolve {
    /// When the data was fetched, in seconds since the Unix epoch.
    saved_at_unix: u64,
    data: ApResolveData,
}

impl ApResolveData {
    // These addresses probably do some geo-location based traffic management or at least DNS-based
    // load balancing. They are known to fail when the normal resolvers are up, so that's why they
    // should only be used as fallback.
    fn fallback() -> Self {
        Self {
            accesspoint: vec![String::from("ap.spotify.com:443")],
            dealer: vec![String::from("dealer.spotify.com:443")],
            spclient: vec![String::from("spclient.wg.spotify.com:443")],
        }
    }
}

impl AccessPoints {
    fn is_any_empty(&self) -> bool {
        self.accesspoint.is_empty() || self.dealer.is_empty() || self.spclient.is_empty()
    }
}

component! {
    ApResolver : ApResolverInner {
        data: AccessPoints = AccessPoints::default(),
    }
}

impl ApResolver {
    // return a port if a proxy URL and/or a proxy port was specified. This is useful even when
    // there is no proxy, but firewalls only allow certain ports (e.g. 443 and not 4070).
    pub fn port_config(&self) -> Option<u16> {
        if self.session().config().proxy.is_some() || self.session().config().ap_port.is_some() {
            Some(self.session().config().ap_port.unwrap_or(443))
        } else {
            None
        }
    }

    fn process_ap_strings(&self, data: Vec<String>) -> VecDeque<SocketAddress> {
        let filter_port = self.port_config();
        data.into_iter()
            .filter_map(|ap| {
                let mut split = ap.rsplitn(2, ':');
                let port = split.next()?;
                let port: u16 = port.parse().ok()?;
                let host = split.next()?.to_owned();
                match filter_port {
                    Some(filter_port) if filter_port != port => None,
                    _ => Some((host, port)),
                }
            })
            .collect()
    }

    fn parse_resolve_to_access_points(&self, resolve: ApResolveData) -> AccessPoints {
        AccessPoints {
            accesspoint: self.process_ap_strings(resolve.accesspoint),
            dealer: self.process_ap_strings(resolve.dealer),
            spclient: self.process_ap_strings(resolve.spclient),
        }
    }

    pub async fn try_apresolve(&self) -> Result<ApResolveData, Error> {
        let req = Request::builder()
            .method(Method::GET)
            .uri("https://apresolve.spotify.com/?type=accesspoint&type=dealer&type=spclient")
            .body(Bytes::new())?;

        let body = self.session().http_client().request_body(req).await?;
        let data: ApResolveData = serde_json::from_slice(body.as_ref())?;

        Ok(data)
    }

    fn load_cached(&self) -> Option<ApResolveData> {
        let session = self.session();
        let cache = session.cache()?;
        let path = cache.apresolve_location()?;
        let bytes = std::fs::read(path).ok()?;
        let cached: CachedResolve = serde_json::from_slice(&bytes).ok()?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        if now.saturating_sub(cached.saved_at_unix) > CACHE_MAX_AGE_SECS {
            return None;
        }
        Some(cached.data)
    }

    fn save_cached(&self, data: &ApResolveData) {
        let session = self.session();
        let Some(cache) = session.cache() else {
            return;
        };
        let Some(path) = cache.apresolve_location() else {
            return;
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let cached = CachedResolve {
            saved_at_unix: now,
            data: data.clone(),
        };
        match serde_json::to_vec(&cached) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    warn!("Cannot save access points to cache: {e}");
                }
            }
            Err(e) => warn!("Cannot serialise access points for the cache: {e}"),
        }
    }

    async fn apresolve(&self) {
        if let Some(cached) = self.load_cached() {
            let data = self.parse_resolve_to_access_points(cached);
            // A cached resolve that filters down to an empty list (for
            // example under a restricted port config) is as useless as a
            // failed one, so let the network resolve replace it.
            if !data.is_any_empty() {
                self.lock(|inner| inner.data = data);
                return;
            }
        }

        let result = self.try_apresolve().await;
        if let Ok(data) = &result {
            self.save_cached(data);
        }

        self.lock(|inner| {
            let (data, error) = match result {
                Ok(data) => (data, None),
                Err(e) => (ApResolveData::default(), Some(e)),
            };

            inner.data = self.parse_resolve_to_access_points(data);

            if inner.data.is_any_empty() {
                warn!("Failed to resolve all access points, using fallbacks");
                if let Some(error) = error {
                    warn!("Resolve access points error: {error}");
                }

                let fallback = self.parse_resolve_to_access_points(ApResolveData::fallback());
                inner.data.accesspoint.extend(fallback.accesspoint);
                inner.data.dealer.extend(fallback.dealer);
                inner.data.spclient.extend(fallback.spclient);
            }
        })
    }

    fn is_any_empty(&self) -> bool {
        self.lock(|inner| inner.data.is_any_empty())
    }

    pub async fn resolve(&self, endpoint: &str) -> Result<SocketAddress, Error> {
        if self.is_any_empty() {
            self.apresolve().await;
        }

        self.lock(|inner| {
            let access_point = match endpoint {
                // take the first position instead of the last with `pop`, because Spotify returns
                // access points with ports 4070, 443 and 80 in order of preference from highest
                // to lowest.
                "accesspoint" => inner.data.accesspoint.pop_front(),
                "dealer" => inner.data.dealer.pop_front(),
                "spclient" => inner.data.spclient.pop_front(),
                _ => {
                    return Err(Error::unimplemented(format!(
                        "No implementation to resolve access point {endpoint}"
                    )));
                }
            };

            let access_point = access_point.ok_or_else(|| {
                Error::unavailable(format!("No access point available for endpoint {endpoint}"))
            })?;

            Ok(access_point)
        })
    }
}
