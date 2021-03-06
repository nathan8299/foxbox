// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

extern crate serde_json;

use foxbox_core::traits::Controller;
use foxbox_taxonomy::manager::*;
use foxbox_taxonomy::api::{API, Error, TargetMap, Targetted, User};
use foxbox_taxonomy::channel::*;
use foxbox_taxonomy::io::*;
use foxbox_taxonomy::values::{format, Binary, Json, Value};
use foxbox_taxonomy::selector::*;
use foxbox_taxonomy::services::*;
use foxbox_taxonomy::util::MimeTypeId;

use foxbox_users::AuthEndpoint;
use foxbox_users::SessionToken;

use iron::{Handler, headers, IronResult, Request, Response};
use iron::headers::ContentType;
use iron::method::Method;
use iron::prelude::Chain;
use iron::request::Body;
use iron::status::Status;

use std::io::{Error as IOError, Read};
use std::sync::Arc;

/// This is a specialized Router for the taxonomy API.
/// It handles all the calls under the api/v1/ url space.
pub struct TaxonomyRouter {
    api: Arc<AdapterManager>,
}

type GetterResultMap = ResultMap<Id<Channel>, Option<(Payload, Arc<Format>)>, Error>;

impl TaxonomyRouter {
    pub fn new(adapter_api: &Arc<AdapterManager>) -> Self {
        TaxonomyRouter { api: adapter_api.clone() }
    }

    fn build_binary_response(&self, payload: &Binary) -> IronResult<Response> {
        use hyper::mime::Mime;

        let mime: Mime = format!("{}", payload.mimetype).parse().unwrap();
        // TODO: stop copying the array here.
        let data = payload.data.clone();

        let mut response = Response::with(data);
        response.status = Some(Status::Ok);
        response.headers.set(ContentType(mime));
        Ok(response)
    }

    fn build_response<S: ToJSON>(&self, obj: S) -> IronResult<Response> {
        let json = obj.to_json();
        let serialized = itry!(serde_json::to_string(&json));
        let mut response = Response::with(serialized);
        response.status = Some(Status::Ok);
        response.headers.set(ContentType::json());
        Ok(response)
    }

    fn build_parse_error(&self, obj: &ParseError) -> IronResult<Response> {
        let mut response = Response::with(itry!(serde_json::to_string(obj)));
        response.status = Some(Status::BadRequest);
        response.headers.set(ContentType::plaintext()); // FIXME: Should be JSON
        Ok(response)
    }

    fn read_body_to_string<'a, 'b: 'a>(body: &mut Body<'a, 'b>) -> Result<String, IOError> {
        let mut s = String::new();
        try!(body.read_to_string(&mut s));
        Ok(s)
    }

    // Checks if a getter result map is a binary payload.
    fn get_binary(&self, map: &GetterResultMap) -> Option<Binary> {
        // For now, consider as binary a result map with a single element that
        // holds a binary value.
        if map.len() != 1 {
            return None;
        }

        for map_value in map.values() {
            if let Ok(Some((ref payload, _))) = *map_value {
                if let Ok(ref data) = payload.to_value(&format::BINARY) {
                    match data.downcast::<Binary>() {
                        Some(data) => {
                            return Some(Binary {
                                mimetype: (*data).mimetype.clone(),
                                data: (*data).data.clone(),
                            });
                        }
                        None => {
                            warn!("get_binary could not convert data labelled as format::BINARY \
                                   to Binary {}",
                                  data.description());
                        }
                    }
                }
                // It's not a binary, proceed as usual.
            }
        }

        None
    }
}

impl Handler for TaxonomyRouter {
    #[allow(cyclomatic_complexity)]
    fn handle(&self, req: &mut Request) -> IronResult<Response> {
        let user: User =
            match req.headers.clone().get::<headers::Authorization<headers::Bearer>>() {
                Some(&headers::Authorization(headers::Bearer { ref token })) => {
                    match SessionToken::from_string(token) {
                        Ok(token) => User::Id(token.claims.id),
                        Err(_) => return Ok(Response::with(Status::Unauthorized)),
                    }
                }
                _ => User::None,
            };

        // We are handling urls relative to the mounter set up in http_server.rs
        // That means that for a full url like http://localhost/api/v1/services
        // the req.url.path will only contain ["services"]
        let path = req.url.path();

        macro_rules! simple_response {
            ($api:ident, $arg:ident, $call:ident) => (self.build_response(&$api.$call($arg, user)))
        }

        macro_rules! binary_response {
            ($api:ident, $arg:ident, $call:ident) => ({
                        let res = $api.$call($arg, user);
                        if let Some(payload) = self.get_binary(&res) {
                            self.build_binary_response(&payload)
                        } else {
                            self.build_response(&res)
                        }
                    })
        }

        // Special case for GET channel/:id
        // This will fetch the values for a ChannelSelector using the id.
        if req.method == Method::Get && path.len() == 2 && path[0] == "channel" {
            let id = Id::<Channel>::new(path[1]);
            let api = &self.api;
            let selector = vec![ChannelSelector::new().with_id(&id)];
            return binary_response!(api, selector, fetch_values);
        }

        // Special case for PUT channel/:id
        // This will send the body to a ChannelSelector using the id.
        if req.method == Method::Put && path.len() == 2 && path[0] == "channel" {
            let id = Id::<Channel>::new(path[1]);
            let api = &self.api;
            let selector = vec![ChannelSelector::new().with_id(&id)];

            let content_type = match req.headers.get::<headers::ContentType>() {
                Some(val) => format!("{}", val),
                None => "application/octet-stream".to_owned(),
            };

            let payload = if content_type.starts_with("application/json") {
                // JSON payload.
                let source = itry!(Self::read_body_to_string(&mut req.body));
                let json = match serde_json::de::from_str(&source as &str) {
                    Err(err) => return self.build_parse_error(&ParseError::json(err)),
                    Ok(args) => args,
                };
                // TODO: check the expected value type for this setter instead of assuming JSON.
                itry!(Payload::from_value(&Value::new(Json(json)), &format::JSON))
            } else {
                // Read a binary payload.
                let mut buffer = Vec::new();
                itry!(req.body.read_to_end(&mut buffer));
                itry!(Payload::from_value(&Value::new(Binary {
                                              data: buffer,
                                              mimetype: Id::<MimeTypeId>::new(&content_type),
                                          }),
                                          &format::BINARY))
            };
            let arg = vec![Targetted {
                               payload: payload,
                               select: selector,
                           }];
            return simple_response!(api, arg, send_values);
        }

        /// Generates the code for a generic HTTP call, where we use an empty
        /// taxonomy selector for GET requests, and a decoded json body for POST ones.
        /// $call is the method we'll call on the api, like get_services.
        /// $sel  is the selector type, like ServiceSelector
        /// $path is a vector describing the url path, like ["service", "tags"]
        macro_rules! get_post_api {
            ($call:ident, $sel:ident, $path:expr) => (
            if path == $path {
                return {
                    match req.method {
                        Method::Get => {
                            // On a GET, just send the full taxonomy content for
                            // this kind of selector.
                            self.build_response(&self.api.$call(vec![$sel::new()]))
                        },
                        Method::Post => {
                            let source = itry!(Self::read_body_to_string(&mut req.body));
                            match Path::new().push_str("body",
                                |path| Vec::<$sel>::from_str_at(path, &source as &str))
                            {
                                Ok(arg) => self.build_response(&self.api.$call(arg)),
                                Err(err) => self.build_parse_error(&err)
                            }
                        },
                        _ => Ok(Response::with((Status::MethodNotAllowed,
                                                format!("Bad method: {}", req.method))))
                    }
                }
            })
        }

        // Generates the code to process a given HTTP call with a json body.
        macro_rules! payload_api {
            ($call:ident, $param:ty, $path:expr, $method:expr, $action:ident) => (
                if path == $path && req.method == $method {
                    type Arg = $param;
                    return {
                        let api = &self.api;
                        let source = itry!(Self::read_body_to_string(&mut req.body));
                        match Path::new().push_str("body",
                            |path| Arg::from_str_at(path, &source as &str))
                        {
                            Ok(arg) => {
                                $action!(api, arg, $call)
                            },
                            Err(err) => self.build_parse_error(&err)
                        }
                    }
                }
            )
        }

        // Generates the code to process a given HTTP call with a json body.
        // This version takes 2 parameters for the internal call.
        macro_rules! payload_api2 {
            ($call:ident, $name1:ident => $param1:ty, $name2:ident => $param2:ty, $path:expr, $method:expr) => (
                if path == $path && req.method == $method {
                    type Param1 = $param1;
                    type Param2 = $param2;
                    return {
                        let source = itry!(Self::read_body_to_string(&mut req.body));
                        let json = match serde_json::de::from_str(&source as &str) {
                            Err(err) => return self.build_parse_error(&ParseError::json(err)),
                            Ok(args) => args
                        };
                        let arg_1 = match Path::new().push_str(&format!("body.{}", stringify!($name1)),
                            |path| Param1::take(path, &json, stringify!($name1))) {
                            Err(err) => return self.build_parse_error(&err),
                            Ok(val) => val
                        };
                        let arg_2 = match Path::new().push_str(&format!("body.{}", stringify!($name2)),
                            |path| Param2::take(path, &json, stringify!($name2))) {
                            Err(err) => return self.build_parse_error(&err),
                            Ok(val) => val
                        };
                        self.build_response(&self.api.$call(arg_1, arg_2))
                    }
                }
            )
        }

        // Keep these urls in sync with the AuthEndpoint(s) in the create() method.

        // Selectors queries.
        get_post_api!(get_services, ServiceSelector, ["services"]);
        get_post_api!(get_channels, ChannelSelector, ["channels"]);

        // Fetching and getting values.
        // We can't use a GET http method here because the Fetch() DOM api
        // doesn't allow bodies with GET and HEAD requests.
        payload_api!(fetch_values, Vec<ChannelSelectorWithFeature>, ["channels", "get"], Method::Put, binary_response);
        payload_api!(send_values, TargetMap<ChannelSelectorWithFeature, Payload>, ["channels", "set"], Method::Put, simple_response);

        // Adding tags.
        payload_api2!(add_service_tags,
                      services => Vec<ServiceSelector>,
                      tags => Vec<Id<TagId>>,
                      ["services", "tags"], Method::Post);
        payload_api2!(add_channel_tags,
                    channels => Vec<ChannelSelector>,
                    tags => Vec<Id<TagId>>,
                    ["channels", "tags"], Method::Post);

        // Removing tags.
        payload_api2!(remove_service_tags,
                      services => Vec<ServiceSelector>,
                      tags => Vec<Id<TagId>>,
                      ["services", "tags"], Method::Delete);
        payload_api2!(remove_channel_tags,
                       channels => Vec<ChannelSelector>,
                       tags => Vec<Id<TagId>>,
                       ["channels", "tags"], Method::Delete);

        // Fallthrough, returning a 404.
        Ok(Response::with((Status::NotFound, format!("Unknown url: {}", req.url))))
    }
}

pub fn create<T>(controller: T,
                 adapter_api: &Arc<AdapterManager>)
                 -> (Chain, Vec<(Vec<Method>, String)>)
    where T: Controller
{
    let router = TaxonomyRouter::new(adapter_api);

    // The list of endpoints supported by this router.
    // Keep it in sync with all the (url path, http method) from
    // the handle() method.
    let endpoints = vec![
        (vec![Method::Get, Method::Post], "services".to_owned()),
        (vec![Method::Post, Method::Delete], "services/tags".to_owned()),
        (vec![Method::Get, Method::Post], "channels".to_owned()),
        (vec![Method::Put], "channels/get".to_owned()),
        (vec![Method::Put], "channels/set".to_owned()),
        (vec![Method::Post, Method::Delete], "channels/tags".to_owned()),
        (vec![Method::Get, Method::Put], "channel/:id".to_owned()),
    ];

    let auth_endpoints = if cfg!(feature = "authentication") && !cfg!(test) {
        endpoints.iter().map(|item| AuthEndpoint(item.0.clone(), item.1.clone())).collect()
    } else {
        vec![]
    };

    let mut chain = Chain::new(router);
    chain.around(controller.get_users_manager().get_middleware(auth_endpoints));

    (chain, endpoints)
}

#[cfg(test)]
describe! taxonomy_router {
    before_each {
        extern crate serde_json;

        use adapters::clock;
        use foxbox_taxonomy::manager::AdapterManager;
        use iron::Headers;
        use iron_test::{ request, response };
        use mount::Mount;
        use stubs::controller::ControllerStub;
        use std::sync::Arc;

        let taxo_manager = Arc::new(AdapterManager::new(None));
        clock::Clock::init(&taxo_manager).unwrap();

        let mut mount = Mount::new();
        mount.mount("/api/v1", create(ControllerStub::new(), &taxo_manager).0);
    }

    it "should return the list of services from a GET request" {
        let response = request::get("http://localhost:3000/api/v1/services",
                                    Headers::new(),
                                    &mount).unwrap();
        let body = response::extract_body_to_string(response);
        let s = r#"[{"adapter":"clock@link.mozilla.org","channels":{"getter:interval.clock@link.mozilla.org":{"adapter":"clock@link.mozilla.org","feature":"clock/time-interval-seconds","id":"getter:interval.clock@link.mozilla.org","service":"service:clock@link.mozilla.org","supports_fetch":null,"supports_send":null,"tags":[]},"getter:timeofday.clock@link.mozilla.org":{"adapter":"clock@link.mozilla.org","feature":"clock/time-of-day-seconds","id":"getter:timeofday.clock@link.mozilla.org","service":"service:clock@link.mozilla.org","supports_fetch":{"returns":{"requires":"Duration (s)"}},"supports_send":null,"tags":[]},"getter:timestamp.clock@link.mozilla.org":{"adapter":"clock@link.mozilla.org","feature":"clock/time-timestamp-rfc-3339","id":"getter:timestamp.clock@link.mozilla.org","service":"service:clock@link.mozilla.org","supports_fetch":{"returns":{"requires":"TimeStamp (RFC 3339)"}},"supports_send":null,"tags":[]}},"id":"service:clock@link.mozilla.org","properties":{"model":"Mozilla clock v1"},"tags":[]}]"#;

        assert_eq!(body, s);
    }

    it "should return the list of services from a POST request" {
        let response = request::post("http://localhost:3000/api/v1/services",
                                    Headers::new(),
                                    r#"[{"id":"service:clock@link.mozilla.org"}]"#,
                                    &mount).unwrap();
        let body = response::extract_body_to_string(response);
        let s = r#"[{"adapter":"clock@link.mozilla.org","channels":{"getter:interval.clock@link.mozilla.org":{"adapter":"clock@link.mozilla.org","feature":"clock/time-interval-seconds","id":"getter:interval.clock@link.mozilla.org","service":"service:clock@link.mozilla.org","supports_fetch":null,"supports_send":null,"tags":[]},"getter:timeofday.clock@link.mozilla.org":{"adapter":"clock@link.mozilla.org","feature":"clock/time-of-day-seconds","id":"getter:timeofday.clock@link.mozilla.org","service":"service:clock@link.mozilla.org","supports_fetch":{"returns":{"requires":"Duration (s)"}},"supports_send":null,"tags":[]},"getter:timestamp.clock@link.mozilla.org":{"adapter":"clock@link.mozilla.org","feature":"clock/time-timestamp-rfc-3339","id":"getter:timestamp.clock@link.mozilla.org","service":"service:clock@link.mozilla.org","supports_fetch":{"returns":{"requires":"TimeStamp (RFC 3339)"}},"supports_send":null,"tags":[]}},"id":"service:clock@link.mozilla.org","properties":{"model":"Mozilla clock v1"},"tags":[]}]"#;

        assert_eq!(body, s);
    }

    it "should return the list of channels from a POST request" {
        let response = request::post("http://localhost:3000/api/v1/channels",
                                     Headers::new(),
                                     r#"[{"id":"getter:interval.clock@link.mozilla.org"}]"#,
                                     &mount).unwrap();
        let body = response::extract_body_to_string(response);
        let s = r#"[{"adapter":"clock@link.mozilla.org","feature":"clock/time-interval-seconds","id":"getter:interval.clock@link.mozilla.org","service":"service:clock@link.mozilla.org","supports_fetch":null,"supports_send":null,"tags":[]}]"#;

        assert_eq!(body, s);
    }
}

#[cfg(test)]
describe! binary_getter {
    it "should return support binary payloads" {
        extern crate serde_json;

        use foxbox_taxonomy::adapter::*;
        use foxbox_taxonomy::channel::*;
        use foxbox_taxonomy::api::{ Error, InternalError, Operation, User };
        use foxbox_taxonomy::manager::AdapterManager;
        use foxbox_taxonomy::services::*;
        use foxbox_taxonomy::values::{ format, Value, Json, Binary };
        use iron::Headers;
        use iron::headers::{ ContentLength, ContentType };
        use iron::status::Status;
        use iron_test::{ request, response };
        use mount::Mount;
        use std::collections::HashMap;
        use std::sync::Arc;
        use stubs::controller::ControllerStub;

        let taxo_manager = Arc::new(AdapterManager::new(None));

// Create a basic adpater and service with a getter returning binary data.

        static ADAPTER_NAME: &'static str = "Test adapter";
        static ADAPTER_VENDOR: &'static str = "team@link.mozilla.org";
        static ADAPTER_VERSION: [u32;4] = [0, 0, 0, 0];

        struct BinaryAdapter { }

        impl Adapter for BinaryAdapter {
            fn id(&self) -> Id<AdapterId> {
                adapter_id!("adapter@test")
            }

            fn name(&self) -> &str {
                ADAPTER_NAME
            }

            fn vendor(&self) -> &str {
                ADAPTER_VENDOR
            }

            fn version(&self) -> &[u32;4] {
                &ADAPTER_VERSION
            }

            fn fetch_values(&self, mut set: Vec<Id<Channel>>, user: User)
                -> ResultMap<Id<Channel>, Option<Value>, Error> {
                assert_eq!(user, User::None);
                set.drain(..).map(|id| {
                    if id == Id::new("getter:binary@link.mozilla.org") {
                        let vec = vec![1, 2, 3, 10, 11, 12];
                        let binary = Binary {
                            data: vec,
                            mimetype: Id::new("image/png")
                        };
                        return (id.clone(), Ok(Some(Value::new(binary))));
                    }

                    (id.clone(), Err(Error::Internal(InternalError::NoSuchChannel(id))))
                }).collect()
            }

            fn send_values(&self, mut values: HashMap<Id<Channel>, Value>, _: User)
                -> ResultMap<Id<Channel>, (), Error> {
                values.drain().map(|(id, value)| {
                    if id == Id::new("setter:binary@link.mozilla.org") {
                        match value.downcast::<Binary>() {
                            Some(payload) => {
                                assert_eq!(payload.mimetype, Id::new("image/png"));
                                let data = &payload.data;
                                assert_eq!(data.len(), 6);
                                assert_eq!(data, &vec![b'A', b'B', b'C', b'D', b'E', b'F']);
                            }
                            None => {
                                panic!(format!("Could not downcast data to Binary {}",
                                value.description()));
                            }
                        }
                        (id.clone(), Ok(()))
                    } else if id == Id::new("setter:json@link.mozilla.org") {
                        match value.downcast::<Json>() {
                            Some(json) => {
                                assert_eq!(serde_json::to_string(&json.to_json()).unwrap(), r#"{"json_body":true}"#);
                            }
                            None => {
                                panic!(format!("Could not downcast data to Binary {}",
                                value.description()));
                            }
                        }
                        (id.clone(), Ok(()))
                    } else{
                        (id.clone(), Err(Error::Internal(InternalError::NoSuchChannel(id))))
                    }
                }).collect()
            }

            fn register_watch(&self, mut watch: Vec<WatchTarget>) -> WatchResult
            {
                watch.drain(..).map(|(id, _, _)| {
                    (id.clone(), Err(Error::OperationNotSupported(Operation::Watch, id)))
                }).collect()
            }
        }

        impl BinaryAdapter {
            fn init(adapt: &Arc<AdapterManager>) -> Result<(), Error> {
                try!(adapt.add_adapter(Arc::new(BinaryAdapter { })));
                let service_id = service_id!("service@test");
                let adapter_id = adapter_id!("adapter@test");
                try!(adapt.add_service(Service::empty(&service_id, &adapter_id)));
                try!(adapt.add_channel(Channel {
                    feature: Id::new("x-test/x-binary"),
                    supports_fetch: Some(Signature::returns(Maybe::Required(format::BINARY.clone()))),
                    id: Id::new("getter:binary@link.mozilla.org"),
                    service: service_id.clone(),
                    adapter: adapter_id.clone(),
                    ..Channel::default()
                }));
                try!(adapt.add_channel(Channel {
                    feature: Id::new("x-test/x-binary"),
                    supports_send: Some(Signature::accepts(Maybe::Required(format::BINARY.clone()))),
                    id: Id::new("setter:binary@link.mozilla.org"),
                    service: service_id.clone(),
                    adapter: adapter_id.clone(),
                    ..Channel::default()
                }));
                try!(adapt.add_channel(Channel {
                    feature: Id::new("x-test/x-binary"),
                    supports_send: Some(Signature::accepts(Maybe::Required(format::JSON.clone()))),
                    id: Id::new("setter:json@link.mozilla.org"),
                    service: service_id.clone(),
                    adapter: adapter_id.clone(),
                    ..Channel::default()
                }));


                Ok(())
            }
        }

        BinaryAdapter::init(&taxo_manager).unwrap();

        let mut mount = Mount::new();
        mount.mount("/api/v1", create(ControllerStub::new(), &taxo_manager).0);

        let response = request::put("http://localhost:3000/api/v1/channels/get",
                                    Headers::new(),
                                    r#"[{"id":"getter:binary@link.mozilla.org", "feature":"x-test/x-binary"}]"#,
                                    &mount).unwrap();

        let content_length = format!("{}", response.headers.get::<ContentLength>().unwrap());
        let content_type = format!("{}", response.headers.get::<ContentType>().unwrap());
        assert_eq!(content_length, "6");
        assert_eq!(content_type, "image/png");

        let result = response::extract_body_to_bytes(response);
        assert_eq!(result, vec![1, 2, 3, 10, 11, 12]);

// Now retrieve the same resource using a GET request.
        let response = request::get("http://localhost:3000/api/v1/channel/getter:binary@link.mozilla.org",
                                    Headers::new(),
                                    &mount).unwrap();

        let content_length = format!("{}", response.headers.get::<ContentLength>().unwrap());
        let content_type = format!("{}", response.headers.get::<ContentType>().unwrap());
        assert_eq!(content_length, "6");
        assert_eq!(content_type, "image/png");

        let result = response::extract_body_to_bytes(response);
        assert_eq!(result, vec![1, 2, 3, 10, 11, 12]);

// Send some binary data to the binary setter.
        let mut headers = Headers::new();
        headers.set(ContentType::png());

        let response = request::put("http://localhost:3000/api/v1/channel/setter:binary@link.mozilla.org",
                                    headers,
                                    "ABCDEF",
                                    &mount).unwrap();

        assert_eq!(response.status, Some(Status::Ok));
        let result = response::extract_body_to_string(response);
        assert_eq!(result, r#"{"setter:binary@link.mozilla.org":null}"#.to_owned());

// Send some json data to the binary setter.
        let mut headers = Headers::new();
        headers.set(ContentType::json());

        let response = request::put("http://localhost:3000/api/v1/channel/setter:json@link.mozilla.org",
                                    headers,
                                    r#"{ "json_body": true }"#,
                                    &mount).unwrap();

        assert_eq!(response.status, Some(Status::Ok));
        let result = response::extract_body_to_string(response);
        assert_eq!(result, r#"{"setter:json@link.mozilla.org":null}"#.to_owned());
    }
}
