use crate::attribute::{get_attribute, store_metadata};
use crate::configuration::FailureMode;
use crate::envoy::{
    Address, AttributeContext, AttributeContext_HttpRequest, AttributeContext_Peer,
    AttributeContext_Request, CheckRequest, CheckResponse_oneof_http_response, Metadata,
    SocketAddress, StatusCode,
};
use crate::service::grpc_message::{GrpcMessageResponse, GrpcMessageResult};
use crate::service::GrpcService;
use chrono::{DateTime, FixedOffset, Timelike};
use log::{debug, warn};
use protobuf::well_known_types::Timestamp;
use protobuf::Message;
use proxy_wasm::hostcalls;
use proxy_wasm::types::{Bytes, MapType};
use std::collections::HashMap;

pub const AUTH_SERVICE_NAME: &str = "envoy.service.auth.v3.Authorization";
pub const AUTH_METHOD_NAME: &str = "Check";

pub struct AuthService;

impl AuthService {
    pub fn request_message(ce_host: String) -> CheckRequest {
        AuthService::build_check_req(ce_host)
    }

    pub fn response_message(res_body_bytes: &Bytes) -> GrpcMessageResult<GrpcMessageResponse> {
        match Message::parse_from_bytes(res_body_bytes) {
            Ok(res) => Ok(GrpcMessageResponse::Auth(res)),
            Err(e) => Err(e),
        }
    }

    fn build_check_req(ce_host: String) -> CheckRequest {
        let mut auth_req = CheckRequest::default();
        let mut attr = AttributeContext::default();
        attr.set_request(AuthService::build_request());
        attr.set_destination(AuthService::build_peer(
            get_attribute::<String>("destination.address").unwrap_or_default(),
            get_attribute::<i64>("destination.port").unwrap_or_default() as u32,
        ));
        attr.set_source(AuthService::build_peer(
            get_attribute::<String>("source.address").unwrap_or_default(),
            get_attribute::<i64>("source.port").unwrap_or_default() as u32,
        ));
        // the ce_host is the identifier for authorino to determine which authconfig to use
        let context_extensions = HashMap::from([("host".to_string(), ce_host)]);
        attr.set_context_extensions(context_extensions);
        attr.set_metadata_context(Metadata::default());
        auth_req.set_attributes(attr);
        auth_req
    }

    fn build_request() -> AttributeContext_Request {
        let mut request = AttributeContext_Request::default();
        let mut http = AttributeContext_HttpRequest::default();
        let headers: HashMap<String, String> = hostcalls::get_map(MapType::HttpRequestHeaders)
            .unwrap()
            .into_iter()
            .collect();

        http.set_host(get_attribute::<String>("request.host").unwrap_or_default());
        http.set_method(get_attribute::<String>("request.method").unwrap_or_default());
        http.set_scheme(get_attribute::<String>("request.scheme").unwrap_or_default());
        http.set_path(get_attribute::<String>("request.path").unwrap_or_default());
        http.set_protocol(get_attribute::<String>("request.protocol").unwrap_or_default());

        http.set_headers(headers);
        request.set_time(get_attribute("request.time").map_or(
            Timestamp::new(),
            |date_time: DateTime<FixedOffset>| Timestamp {
                nanos: date_time.nanosecond() as i32,
                seconds: date_time.second() as i64,
                unknown_fields: Default::default(),
                cached_size: Default::default(),
            },
        ));
        request.set_http(http);
        request
    }

    fn build_peer(host: String, port: u32) -> AttributeContext_Peer {
        let mut peer = AttributeContext_Peer::default();
        let mut address = Address::default();
        let mut socket_address = SocketAddress::default();
        socket_address.set_address(host);
        socket_address.set_port_value(port);
        address.set_socket_address(socket_address);
        peer.set_address(address);
        peer
    }

    pub fn process_auth_grpc_response(
        auth_resp: GrpcMessageResponse,
        failure_mode: &FailureMode,
    ) -> Result<(), StatusCode> {
        if let GrpcMessageResponse::Auth(check_response) = auth_resp {
            // store dynamic metadata in filter state
            store_metadata(check_response.get_dynamic_metadata());

            match check_response.http_response {
                Some(CheckResponse_oneof_http_response::ok_response(ok_response)) => {
                    debug!("process_auth_grpc_response: received OkHttpResponse");
                    ok_response
                        .get_response_headers_to_add()
                        .iter()
                        .for_each(|header| {
                            hostcalls::add_map_value(
                                MapType::HttpResponseHeaders,
                                header.get_header().get_key(),
                                header.get_header().get_value(),
                            )
                            .unwrap()
                        });
                    Ok(())
                }
                Some(CheckResponse_oneof_http_response::denied_response(denied_response)) => {
                    debug!("process_auth_grpc_response: received DeniedHttpResponse",);

                    let mut response_headers = vec![];
                    let status_code = denied_response.get_status().code;
                    denied_response.get_headers().iter().for_each(|header| {
                        response_headers.push((
                            header.get_header().get_key(),
                            header.get_header().get_value(),
                        ))
                    });
                    hostcalls::send_http_response(
                        status_code as u32,
                        response_headers,
                        Some(denied_response.get_body().as_ref()),
                    )
                    .unwrap();
                    Err(status_code)
                }
                None => {
                    GrpcService::handle_error_on_grpc_response(failure_mode);
                    Err(StatusCode::InternalServerError)
                }
            }
        } else {
            warn!("not a GrpcMessageResponse::Auth(CheckResponse)!");
            GrpcService::handle_error_on_grpc_response(failure_mode);
            Err(StatusCode::InternalServerError)
        }
    }
}
