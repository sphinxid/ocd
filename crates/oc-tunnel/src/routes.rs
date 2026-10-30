//! Routes (`client/src/effect/routes.ts`, and `resolveRoute` in `tunnel.ts`): a route name (`@` for the tunnel
//! hostname, or a subdomain label) maps to a `host:port` target, optionally with a PROXY protocol version.

use std::collections::BTreeMap;

use crate::names::{self, Target};
use crate::proxy_protocol;

/// A route's target with options (`OpenTunnelRoute`; a plain `host:port` string is a route without options).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Route {
    /// `host:port` of the local service.
    pub target: String,
    /// `"v1"` or `"v2"`: writes a PROXY protocol header carrying the visitor's address to the target before each
    /// connection's data. Only for targets that expect one.
    pub proxy_protocol: Option<String>,
}

impl Route {
    pub fn new(target: impl Into<String>) -> Route {
        Route { target: target.into(), proxy_protocol: None }
    }

    pub fn with_proxy_protocol(target: impl Into<String>, version: impl Into<String>) -> Route {
        Route { target: target.into(), proxy_protocol: Some(version.into()) }
    }
}

impl From<String> for Route {
    fn from(target: String) -> Route {
        Route::new(target)
    }
}

impl From<&str> for Route {
    fn from(target: &str) -> Route {
        Route::new(target)
    }
}

/// A route serializes as its target, or as `{target, proxyProtocol}` with a PROXY protocol version.
impl serde::Serialize for Route {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.proxy_protocol {
            None => serializer.serialize_str(&self.target),
            Some(version) => {
                use serde::ser::SerializeStruct;
                let mut s = serializer.serialize_struct("Route", 2)?;
                s.serialize_field("target", &self.target)?;
                s.serialize_field("proxyProtocol", version)?;
                s.end()
            }
        }
    }
}

/// Route name to its target (`OpenTunnelRoutes`).
pub type Routes = BTreeMap<String, Route>;

/// Routes of plain `host:port` targets.
pub fn routes<K: Into<String>, V: Into<String>>(pairs: impl IntoIterator<Item = (K, V)>) -> Routes {
    pairs.into_iter().map(|(name, target)| (name.into(), Route::new(target))).collect()
}

/// A validated route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub target: Target,
    pub proxy_protocol: Option<String>,
}

/// `resolveRoute`: validates one route; the error is the message the SDK throws.
pub fn resolve_route(name: &str, route: &Route) -> Result<Resolved, String> {
    if !names::is_valid_route(name) {
        return Err(format!("Invalid route name '{name}'"));
    }
    if let Some(version) = &route.proxy_protocol
        && !proxy_protocol::is_version(version)
    {
        return Err(format!("Invalid proxyProtocol '{version}' for route '{name}': use \"v1\" or \"v2\""));
    }
    let target = names::parse_target(&route.target)
        .ok_or_else(|| format!("Invalid target '{}' for route '{name}': use host:port", route.target))?;
    Ok(Resolved { target, proxy_protocol: route.proxy_protocol.clone() })
}

/// `resolveRoutes`
pub fn resolve_routes(routes: &Routes) -> Result<BTreeMap<String, Resolved>, String> {
    routes.iter().map(|(name, route)| Ok((name.clone(), resolve_route(name, route)?))).collect()
}

/// `validateRoutes`
pub fn validate_routes(routes: &Routes) -> Result<(), String> {
    resolve_routes(routes).map(|_| ())
}

/// `publicUrl`: the public URL of a route on a tunnel hostname.
pub fn public_url(route: &str, hostname: &str) -> String {
    format!("https://{}", names::public_hostname(route, hostname))
}

/// `publicUrls`: the public URL of each route on a tunnel hostname.
pub fn public_urls(routes: &Routes, hostname: &str) -> BTreeMap<String, String> {
    routes.keys().map(|route| (route.clone(), public_url(route, hostname))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_are_validated_like_the_sdk() {
        // Messages as the SDK throws them (compared under Bun 1.4.4).
        assert_eq!(resolve_route("Bad_Name", &"127.0.0.1:1".into()).unwrap_err(), "Invalid route name 'Bad_Name'");
        assert_eq!(
            resolve_route("r1", &"nope".into()).unwrap_err(),
            "Invalid target 'nope' for route 'r1': use host:port"
        );
        assert_eq!(
            resolve_route("r1", &Route::with_proxy_protocol("nope", "v3")).unwrap_err(),
            "Invalid proxyProtocol 'v3' for route 'r1': use \"v1\" or \"v2\""
        );
        let resolved = resolve_route("@", &Route::with_proxy_protocol("[::1]:80", "v2")).unwrap();
        assert_eq!(resolved.target, Target { host: "::1".into(), port: 80 });
        assert_eq!(resolved.proxy_protocol.as_deref(), Some("v2"));
    }

    #[test]
    fn public_urls_name_each_route() {
        let routes = routes([("@", "h:1"), ("api", "h:2")]);
        let urls = public_urls(&routes, "abc.opentunnel.test");
        assert_eq!(urls["@"], "https://abc.opentunnel.test");
        assert_eq!(urls["api"], "https://api.abc.opentunnel.test");
    }
}
