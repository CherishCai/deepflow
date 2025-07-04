/*
 * Copyright (c) 2024 Yunshan Networks
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use public::bytes::read_u16_be;

use public::l7_protocol::L7Protocol;
use serde::Serialize;

use crate::common::flow::{L7PerfStats, PacketDirection};
use crate::common::l7_protocol_info::{L7ProtocolInfo, L7ProtocolInfoInterface};
use crate::common::l7_protocol_log::{L7ParseResult, L7ProtocolParserInterface, ParseParam};
use crate::common::meta_packet::EbpfFlags;
use crate::config::handler::{L7LogDynamicConfig, LogParserConfig};
use crate::flow_generator::protocol_logs::{set_captured_byte, value_is_default};
use crate::flow_generator::{Error, Result};

use super::consts::{
    HTTP_STATUS_CLIENT_ERROR_MAX, HTTP_STATUS_CLIENT_ERROR_MIN, HTTP_STATUS_SERVER_ERROR_MAX,
    HTTP_STATUS_SERVER_ERROR_MIN,
};
use super::{
    check_http_method, parse_v1_headers,
    pb_adapter::{ExtendedInfo, L7ProtocolSendLog, L7Request, L7Response, TraceInfo},
    AppProtoHead, L7ResponseStatus, LogMessageType, PrioField,
};

const BASE_FIELD_PRIORITY: u8 = 0;

const FCGI_RECORD_FIX_LEN: usize = 8;

const FCGI_BEGIN_REQUEST: u8 = 1;
const FCGI_ABORT_REQUEST: u8 = 2;
const FCGI_END_REQUEST: u8 = 3;
const FCGI_PARAMS: u8 = 4;
const FCGI_STDIN: u8 = 5;
const FCGI_STDOUT: u8 = 6;
const FCGI_STDERR: u8 = 7;
const FCGI_DATA: u8 = 8;
const FCGI_GET_VALUES: u8 = 9;
const FCGI_GET_VALUES_RESULT: u8 = 10;
const FCGI_UNKNOWN_TYPE: u8 = 11;
const FCGI_MAXTYPE: u8 = FCGI_UNKNOWN_TYPE;

#[derive(Serialize, Debug, Default, Clone, PartialEq, Eq)]
pub struct FastCGIInfo {
    version: u8,
    #[serde(rename = "request_id", skip_serializing_if = "value_is_default")]
    request_id: u32,
    msg_type: LogMessageType,
    #[serde(rename = "request_type", skip_serializing_if = "value_is_default")]
    pub method: String,
    #[serde(rename = "request_resource", skip_serializing_if = "value_is_default")]
    pub path: String,
    #[serde(rename = "request_domain", skip_serializing_if = "value_is_default")]
    pub host: String,
    #[serde(rename = "user_agent", skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(rename = "endpoint", skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(rename = "response_code", skip_serializing_if = "Option::is_none")]
    pub status_code: Option<i32>,
    #[serde(rename = "response_status")]
    status: L7ResponseStatus,

    #[serde(rename = "request_length", skip_serializing_if = "Option::is_none")]
    pub req_content_length: Option<u32>,
    #[serde(rename = "response_length", skip_serializing_if = "Option::is_none")]
    pub resp_content_length: Option<u32>,

    #[serde(skip_serializing_if = "value_is_default")]
    pub trace_id: PrioField<String>,
    #[serde(skip_serializing_if = "value_is_default")]
    pub span_id: PrioField<String>,

    #[serde(skip_serializing_if = "value_is_default")]
    pub x_request_id_0: PrioField<String>,
    #[serde(skip_serializing_if = "value_is_default")]
    pub x_request_id_1: PrioField<String>,

    captured_request_byte: u32,
    captured_response_byte: u32,

    #[serde(skip)]
    rrt: u64,

    #[serde(skip)]
    is_tls: bool,

    #[serde(skip)]
    seq_off: u32,

    #[serde(skip)]
    is_on_blacklist: bool,
}

impl L7ProtocolInfoInterface for FastCGIInfo {
    fn session_id(&self) -> Option<u32> {
        Some(self.request_id)
    }

    fn merge_log(&mut self, other: &mut L7ProtocolInfo) -> Result<()> {
        if let L7ProtocolInfo::FastCGIInfo(info) = other {
            self.status = info.status;
            self.status_code = info.status_code;
            self.captured_response_byte = info.captured_response_byte;
            super::swap_if!(self, trace_id, is_default, info);
            super::swap_if!(self, span_id, is_default, info);
            if info.is_on_blacklist {
                self.is_on_blacklist = info.is_on_blacklist;
            }
        }

        Ok(())
    }

    fn app_proto_head(&self) -> Option<AppProtoHead> {
        Some(AppProtoHead {
            proto: L7Protocol::FastCGI,
            msg_type: self.msg_type,
            rrt: self.rrt,
        })
    }

    fn is_tls(&self) -> bool {
        self.is_tls
    }

    fn tcp_seq_offset(&self) -> u32 {
        self.seq_off
    }

    fn get_request_domain(&self) -> String {
        self.host.clone()
    }

    fn get_endpoint(&self) -> Option<String> {
        self.endpoint.clone()
    }

    fn get_request_resource_length(&self) -> usize {
        self.path.len()
    }

    fn is_on_blacklist(&self) -> bool {
        self.is_on_blacklist
    }
}

impl FastCGIInfo {
    // reference https://www.mit.edu/~yandros/doc/specs/fcgi-spec.html#S3.4
    fn fill_from_param(
        &mut self,
        param_payload: &[u8],
        direction: PacketDirection,
        config: Option<&L7LogDynamicConfig>,
    ) -> Result<()> {
        let mut p = param_payload;
        while p.len() > 2 {
            let Ok((off, key_len, val_len)) = read_param_kv_len(p) else {
                break;
            };
            p = &p[off..];

            if p.len() < key_len + val_len {
                break;
            }

            let (key, val) = (&p[..key_len], &p[key_len..key_len + val_len]);
            self.on_param(key, val, direction, config)?;

            p = &p[(key_len + val_len)..]
        }

        Ok(())
    }

    fn on_param(
        &mut self,
        key: &[u8],
        val: &[u8],
        direction: PacketDirection,
        config: Option<&L7LogDynamicConfig>,
    ) -> Result<()> {
        match key {
            b"REQUEST_METHOD" => self.method = String::from_utf8_lossy(val).to_string(),
            b"CONTENT_LENGTH" => {
                if val.len() != 0 {
                    let l = std::str::from_utf8(val)
                        .map_err(|_| Error::L7ProtocolUnknown)?
                        .parse::<u32>()
                        .map_err(|_| Error::L7ProtocolUnknown)?;
                    match direction {
                        PacketDirection::ClientToServer => self.req_content_length = Some(l),
                        PacketDirection::ServerToClient => self.resp_content_length = Some(l),
                    }
                }
            }
            b"SCRIPT_NAME" => {
                if self.path.is_empty() {
                    self.path = String::from_utf8_lossy(val).to_string()
                }
            }
            b"REQUEST_URI" => self.path = String::from_utf8_lossy(val).to_string(),
            b"HTTP_HOST" => self.host = String::from_utf8_lossy(val).to_string(),
            b"HTTP_USER_AGENT" => self.user_agent = Some(String::from_utf8_lossy(val).to_string()),
            b"DOCUMENT_URI" => self.endpoint = Some(String::from_utf8_lossy(val).to_string()),
            _ => {
                let Some(config) = config else {
                    return Ok(());
                };

                // value must be valid utf8 from here
                let (Ok(key), Ok(val)) = (std::str::from_utf8(key), std::str::from_utf8(val))
                else {
                    return Ok(());
                };
                let lower_key = key.to_lowercase();
                let key = lower_key.as_str();

                if config.is_trace_id(key) {
                    for (i, trace) in config.trace_types.iter().enumerate() {
                        let prio = i as u8 + BASE_FIELD_PRIORITY;
                        if self.trace_id.prio <= prio {
                            break;
                        }
                        if !trace.check(key) {
                            continue;
                        }
                        trace
                            .decode_trace_id(val)
                            .map(|id| self.trace_id = PrioField::new(prio, id.to_string()));
                    }
                }

                if config.is_span_id(key) {
                    for (i, span) in config.span_types.iter().enumerate() {
                        let prio = i as u8 + BASE_FIELD_PRIORITY;
                        if self.span_id.prio <= prio {
                            break;
                        }
                        if !span.check(key) {
                            continue;
                        }
                        span.decode_span_id(val)
                            .map(|id| self.span_id = PrioField::new(prio, id.to_string()));
                    }
                }

                let x_req_id = if direction == PacketDirection::ClientToServer {
                    &mut self.x_request_id_0
                } else {
                    &mut self.x_request_id_1
                };
                for (i, req_id) in config.x_request_id.iter().enumerate() {
                    let prio = i as u8 + BASE_FIELD_PRIORITY;
                    if x_req_id.prio <= prio {
                        break;
                    }
                    if req_id == key {
                        *x_req_id = PrioField::new(prio, val.to_owned());
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    fn set_is_on_blacklist(&mut self, config: &LogParserConfig) {
        if let Some(t) = config.l7_log_blacklist_trie.get(&L7Protocol::FastCGI) {
            self.is_on_blacklist = t.request_resource.is_on_blacklist(&self.path)
                || t.request_type.is_on_blacklist(&self.method)
                || t.request_domain.is_on_blacklist(&self.host)
                || self
                    .endpoint
                    .as_ref()
                    .map(|p| t.endpoint.is_on_blacklist(p))
                    .unwrap_or_default();
        }
    }
}

impl From<FastCGIInfo> for L7ProtocolSendLog {
    fn from(f: FastCGIInfo) -> Self {
        let flags = if f.is_tls {
            EbpfFlags::TLS.bits()
        } else {
            EbpfFlags::NONE.bits()
        };
        Self {
            captured_request_byte: f.captured_request_byte,
            captured_response_byte: f.captured_response_byte,
            req: L7Request {
                req_type: f.method,
                domain: f.host,
                resource: f.path,
                endpoint: f.endpoint.unwrap_or_default(),
            },
            resp: L7Response {
                status: f.status,
                code: f.status_code,
                ..Default::default()
            },
            version: Some(f.version.to_string()),
            trace_info: Some(TraceInfo {
                trace_id: if f.trace_id.is_default() {
                    None
                } else {
                    Some(f.trace_id.into_inner())
                },
                span_id: if f.span_id.is_default() {
                    None
                } else {
                    Some(f.span_id.into_inner())
                },
                ..Default::default()
            }),
            ext_info: Some(ExtendedInfo {
                x_request_id_0: Some(f.x_request_id_0.into_inner()),
                x_request_id_1: Some(f.x_request_id_1.into_inner()),
                request_id: Some(f.request_id),
                ..Default::default()
            }),
            flags,
            ..Default::default()
        }
    }
}

// reference https://www.mit.edu/~yandros/doc/specs/fcgi-spec.html#S3.3
struct FastCGIRecord {
    version: u8,
    record_type: u8,
    request_id: u16,
    content_len: u16,
    padding_len: u8,
}

impl FastCGIRecord {
    fn parse(payload: &[u8]) -> Result<Self> {
        if payload.len() < FCGI_RECORD_FIX_LEN {
            return Err(Error::L7ProtocolUnknown);
        }
        let r = Self {
            version: payload[0],
            record_type: payload[1],
            request_id: read_u16_be(&payload[2..]),
            content_len: read_u16_be(&payload[4..]),
            padding_len: payload[6],
        };
        if r.record_type > FCGI_MAXTYPE {
            return Err(Error::L7ProtocolUnknown);
        }
        Ok(r)
    }
}

#[derive(Default)]
pub struct FastCGILog {
    perf_stats: Option<L7PerfStats>,
    last_is_on_blacklist: bool,
}

impl FastCGILog {
    fn set_status(&mut self, status_code: u16, info: &mut FastCGIInfo) {
        if status_code >= HTTP_STATUS_CLIENT_ERROR_MIN
            && status_code <= HTTP_STATUS_CLIENT_ERROR_MAX
        {
            // http客户端请求存在错误
            info.status = L7ResponseStatus::ClientError;
        } else if status_code >= HTTP_STATUS_SERVER_ERROR_MIN
            && status_code <= HTTP_STATUS_SERVER_ERROR_MAX
        {
            info.status = L7ResponseStatus::ServerError;
        } else {
            info.status = L7ResponseStatus::Ok;
        }
    }
}

struct RecordIter<'a> {
    p: &'a [u8],
    accumulate_off: usize,
}

impl<'a> RecordIter<'a> {
    fn new(p: &'a [u8]) -> Self {
        Self {
            p: p,
            accumulate_off: 0,
        }
    }
}

impl<'a> Iterator for RecordIter<'a> {
    // record, record_data, offset
    type Item = (FastCGIRecord, &'a [u8], usize);

    fn next(&mut self) -> Option<Self::Item> {
        let off = self.accumulate_off;
        let Ok(r) = FastCGIRecord::parse(self.p) else {
            return None;
        };

        let content_end = FCGI_RECORD_FIX_LEN + r.content_len as usize;
        let content = if content_end > self.p.len() {
            &self.p[FCGI_RECORD_FIX_LEN..]
        } else {
            &self.p[FCGI_RECORD_FIX_LEN..content_end]
        };
        let record_end = content_end + r.padding_len as usize;

        self.p = if record_end > self.p.len() {
            &self.p[self.p.len()..]
        } else {
            &self.p[record_end..]
        };
        self.accumulate_off += record_end;
        Some((r, content, off))
    }
}

impl L7ProtocolParserInterface for FastCGILog {
    fn check_payload(&mut self, payload: &[u8], _: &ParseParam) -> bool {
        for (r, p, _) in RecordIter::new(payload) {
            match r.record_type {
                FCGI_END_REQUEST | FCGI_STDOUT => return false,
                _ => {}
            }

            if r.record_type == FCGI_PARAMS {
                if let Ok(val) = get_param_val(p, "REQUEST_METHOD") {
                    if check_http_method(
                        std::str::from_utf8(val)
                            .map_err(|_| Error::L7ProtocolUnknown)
                            .unwrap_or(""),
                    )
                    .is_ok()
                    {
                        return true;
                    }
                }
                return false;
            }
        }

        false
    }

    fn parse_payload(&mut self, payload: &[u8], param: &ParseParam) -> Result<L7ParseResult> {
        let config = param.parse_config.and_then(|c| Some(&c.l7_log_dynamic));
        if self.perf_stats.is_none() {
            self.perf_stats = Some(L7PerfStats::default())
        }

        let mut info = FastCGIInfo::default();

        match param.direction {
            PacketDirection::ClientToServer => {
                info.msg_type = LogMessageType::Request;
                for (record, record_payload, off) in RecordIter::new(payload) {
                    if record.record_type == FCGI_PARAMS {
                        info.request_id = record.request_id as u32;
                        info.version = record.version;
                        if record.content_len > 0 {
                            info.fill_from_param(record_payload, param.direction, config)?;
                            info.seq_off = off as u32;
                            break;
                        }
                    }
                }

                if info.method.is_empty() {
                    return Err(Error::L7ProtocolUnknown);
                }
            }
            PacketDirection::ServerToClient => {
                info.msg_type = LogMessageType::Response;

                'l: for (record, record_payload, off) in RecordIter::new(payload) {
                    info.seq_off = off as u32;
                    if record.record_type == FCGI_STDOUT {
                        info.request_id = record.request_id as u32;
                        info.version = record.version;
                        let mut is_hdr = false;

                        for i in parse_v1_headers(record_payload) {
                            let Some(col_index) = i.find(':') else {
                                break;
                            };

                            if col_index + 1 >= i.len() {
                                break;
                            }

                            is_hdr = true;
                            let key = &i[..col_index];
                            let value = &i[col_index + 1..];
                            info.on_param(
                                key.as_bytes(),
                                value.as_bytes(),
                                PacketDirection::ServerToClient,
                                config,
                            )?;

                            if key == "Status" {
                                if value.len() < 4 {
                                    break 'l;
                                }
                                if let Ok(status_code) = &value[1..4].parse::<u16>() {
                                    info.status_code = Some(*status_code as i32);
                                    self.set_status(*status_code, &mut info);
                                };
                                break 'l;
                            }
                        }

                        if !is_hdr {
                            continue;
                        }

                        if info.status_code.is_none() {
                            info.status_code = Some(200);
                            self.set_status(200, &mut info);
                        }
                        break 'l;
                    }
                }

                if info.status_code.is_none() {
                    return Err(Error::L7ProtocolUnknown);
                }
            }
        }
        info.is_tls = param.is_tls();
        set_captured_byte!(info, param);
        if let Some(config) = param.parse_config {
            info.set_is_on_blacklist(config);
        }
        if !info.is_on_blacklist && !self.last_is_on_blacklist {
            match param.direction {
                PacketDirection::ClientToServer => {
                    self.perf_stats.as_mut().map(|p| p.inc_req());
                }
                PacketDirection::ServerToClient => {
                    self.perf_stats.as_mut().map(|p| p.inc_resp());
                    if info.status == L7ResponseStatus::ClientError {
                        self.perf_stats.as_mut().map(|p| p.inc_req_err());
                    } else if info.status == L7ResponseStatus::ServerError {
                        self.perf_stats.as_mut().map(|p| p.inc_resp_err());
                    }
                }
            }
            info.cal_rrt(param, &info.endpoint).map(|(rrt, endpoint)| {
                info.rrt = rrt;
                if info.msg_type == LogMessageType::Response {
                    info.endpoint = endpoint;
                }
                self.perf_stats.as_mut().map(|p| p.update_rrt(rrt));
            });
        }
        self.last_is_on_blacklist = info.is_on_blacklist;
        Ok(L7ParseResult::Single(L7ProtocolInfo::FastCGIInfo(info)))
    }

    fn protocol(&self) -> L7Protocol {
        L7Protocol::FastCGI
    }

    fn perf_stats(&mut self) -> Option<L7PerfStats> {
        self.perf_stats.take()
    }

    fn parsable_on_udp(&self) -> bool {
        false
    }
}

// return (offset, key_len, val_len)
fn read_param_kv_len(param_payload: &[u8]) -> Result<(usize, usize, usize)> {
    let mut p = param_payload;
    if p.len() < 2 {
        return Err(Error::L7ProtocolUnknown);
    }
    let mut off = 0;
    let (key_len, val_len);

    let key_len_b = p[0];
    if key_len_b >> 7 == 0 {
        key_len = key_len_b as usize;
        p = &p[1..];
        off += 1;
    } else {
        if p.len() < 4 {
            return Err(Error::L7ProtocolUnknown);
        }
        key_len = ((key_len_b as usize & 0x7f) << 24)
            + ((p[1] as usize) << 16)
            + ((p[2] as usize) << 8)
            + p[3] as usize;
        p = &p[4..];
        off += 4;
    }

    if p.is_empty() {
        return Err(Error::L7ProtocolUnknown);
    }

    let val_len_b = p[0];
    if val_len_b >> 7 == 0 {
        val_len = val_len_b as usize;
        off += 1;
    } else {
        if p.len() < 4 {
            return Err(Error::L7ProtocolUnknown);
        }
        val_len = ((val_len_b as usize & 0x7f) << 24)
            + ((p[1] as usize) << 16)
            + ((p[2] as usize) << 8)
            + p[3] as usize;
        off += 4;
    }

    return Ok((off, key_len, val_len));
}

fn get_param_val<'a>(param_payload: &'a [u8], key: &str) -> Result<&'a [u8]> {
    let mut p = param_payload;
    while p.len() > 2 {
        let Ok((off, key_len, val_len)) = read_param_kv_len(p) else {
            break;
        };
        p = &p[off..];

        if p.len() < key_len + val_len {
            return Err(Error::L7ProtocolUnknown);
        }

        let (k, val) = (&p[..key_len], &p[key_len..key_len + val_len]);
        if k == key.as_bytes() {
            return Ok(val);
        }

        p = &p[(key_len + val_len)..];
    }
    return Err(Error::L7ProtocolUnknown);
}

#[cfg(test)]
mod test {
    use std::{cell::RefCell, path::Path, rc::Rc};

    use crate::common::flow::{L7PerfStats, PacketDirection};
    use crate::common::l7_protocol_info::{L7ProtocolInfo, L7ProtocolInfoInterface};
    use crate::common::l7_protocol_log::{L7PerfCache, L7ProtocolParserInterface, ParseParam};
    use crate::flow_generator::protocol_logs::fastcgi::FastCGILog;
    use crate::flow_generator::protocol_logs::L7ResponseStatus;
    use crate::flow_generator::LogMessageType;
    use crate::{flow_generator::L7_RRT_CACHE_CAPACITY, utils::test::Capture};

    use super::FastCGIInfo;

    const FILE_DIR: &str = "resources/test/flow_generator/fastcgi";

    #[test]
    fn test_fastcgi() {
        let (info, perf) = check_and_parse("fastcgi.pcap");
        assert_eq!(info.method.as_str(), "GET");

        let f = FastCGIInfo {
            version: 1,
            request_id: 1,
            msg_type: LogMessageType::Request,
            method: "GET".into(),
            path: "/aaaaa".into(),
            host: "172.17.0.3:8080".into(),
            user_agent: Some("curl/7.87.0".into()),
            endpoint: Some("/index.php".into()),
            status_code: Some(200),
            status: L7ResponseStatus::Ok,
            seq_off: 16,
            captured_request_byte: 576,
            captured_response_byte: 88,
            ..Default::default()
        };

        assert_eq!(info, f);

        assert_eq!(
            perf,
            L7PerfStats {
                request_count: 1,
                response_count: 1,
                err_client_count: 0,
                err_server_count: 0,
                err_timeout: 0,
                rrt_count: 1,
                rrt_sum: 174,
                rrt_max: 174,
                ..Default::default()
            }
        );
    }

    fn check_and_parse(file_name: &str) -> (FastCGIInfo, L7PerfStats) {
        let pcap_file = Path::new(FILE_DIR).join(file_name);
        let capture = Capture::load_pcap(pcap_file);
        let log_cache = Rc::new(RefCell::new(L7PerfCache::new(L7_RRT_CACHE_CAPACITY)));
        let mut p = capture.collect::<Vec<_>>();
        p[0].lookup_key.direction = PacketDirection::ClientToServer;
        p[1].lookup_key.direction = PacketDirection::ServerToClient;

        let mut parser = FastCGILog::default();
        let req_param = &mut ParseParam::new(
            &p[0],
            log_cache.clone(),
            Default::default(),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Default::default(),
            true,
            true,
        );
        let req_payload = p[0].get_l4_payload().unwrap();
        req_param.set_captured_byte(req_payload.len());
        assert_eq!((&mut parser).check_payload(req_payload, req_param), true);
        let info = (&mut parser).parse_payload(req_payload, req_param).unwrap();
        let mut req = info.unwrap_single();

        (&mut parser).reset();

        let resp_param = &mut ParseParam::new(
            &p[1],
            log_cache.clone(),
            Default::default(),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Default::default(),
            true,
            true,
        );
        let resp_payload = p[1].get_l4_payload().unwrap();
        resp_param.set_captured_byte(resp_payload.len());
        assert_eq!((&mut parser).check_payload(resp_payload, resp_param), false);
        let mut resp = (&mut parser)
            .parse_payload(resp_payload, resp_param)
            .unwrap()
            .unwrap_single();

        req.merge_log(&mut resp).unwrap();
        if let L7ProtocolInfo::FastCGIInfo(info) = req {
            return (info, parser.perf_stats.unwrap());
        }
        unreachable!()
    }
}
