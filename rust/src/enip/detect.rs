/* Copyright (C) 2023 Open Information Security Foundation
 *
 * You can copy, redistribute or modify this Program under the terms of
 * the GNU General Public License version 2 as published by the Free
 * Software Foundation.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * version 2 along with this program; if not, write to the Free Software
 * Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA
 * 02110-1301, USA.
 */

use nom7::character::complete::{char, digit1, space0};
use nom7::combinator::{map_opt, opt, verify};
use nom7::error::{make_error, ErrorKind};
use nom7::IResult;

use std::os::raw::{c_int, c_void};

use super::constant::{EnipCommand, EnipStatus};
use super::enip::{EnipTransaction, ALPROTO_ENIP};
use super::parser::{
    CipData, CipDir, EnipCipRequestPayload, EnipCipResponsePayload, EnipItemPayload, EnipPayload,
    CIP_MULTIPLE_SERVICE,
};

use crate::core::{STREAM_TOCLIENT, STREAM_TOSERVER};
use crate::detect::uint::{
    detect_match_uint, detect_parse_uint_enum, DetectUintData, SCDetectU16Free, SCDetectU16Match,
    SCDetectU16Parse, SCDetectU32Free, SCDetectU32Match, SCDetectU32Parse, SCDetectU8Free,
    SCDetectU8Match, SCDetectU8Parse,
};
use crate::detect::{helper_keyword_register_sticky_buffer, SigTableElmtStickyBuffer};
use suricata_sys::sys::{
    DetectEngineCtx, DetectEngineThreadCtx, Flow, SCDetectBufferSetActiveList,
    SCDetectHelperBufferMpmRegister, SCDetectHelperBufferRegister, SCDetectHelperKeywordRegister,
    SCDetectSignatureSetAppProto, SCSigMatchAppendSMToList, SCSigTableAppLiteElmt, SigMatchCtx,
    Signature,
};

use crate::direction::Direction;

use std::ffi::CStr;

unsafe fn parse_command(raw: *const std::os::raw::c_char) -> *mut DetectUintData<u16> {
    let raw: &CStr = CStr::from_ptr(raw); //unsafe
    if let Ok(s) = raw.to_str() {
        if let Some(ctx) = detect_parse_uint_enum::<u16, EnipCommand>(s) {
            let boxed = Box::new(ctx);
            return Box::into_raw(boxed) as *mut _;
        }
    }
    return std::ptr::null_mut();
}

unsafe fn parse_status(raw: *const std::os::raw::c_char) -> *mut DetectUintData<u32> {
    let raw: &CStr = CStr::from_ptr(raw); //unsafe
    if let Ok(s) = raw.to_str() {
        if let Some(ctx) = detect_parse_uint_enum::<u32, EnipStatus>(s) {
            let boxed = Box::new(ctx);
            return Box::into_raw(boxed) as *mut _;
        }
    }
    return std::ptr::null_mut();
}

#[derive(Clone, Debug, Default)]
pub struct DetectCipServiceData {
    pub service: u8,
    pub class: Option<u32>,
    pub attribute: Option<u32>,
}

fn enip_parse_cip_service(i: &str) -> IResult<&str, DetectCipServiceData> {
    let (i, _) = space0(i)?;
    let (i, service) = verify(map_opt(digit1, |s: &str| s.parse::<u8>().ok()), |&v| {
        v < 0x80
    })(i)?;
    let mut class = None;
    let mut attribute = None;
    let (i, _) = space0(i)?;
    let (i, comma) = opt(char(','))(i)?;
    let mut input = i;
    if comma.is_some() {
        let (i, _) = space0(i)?;
        let (i, class1) = map_opt(digit1, |s: &str| s.parse::<u32>().ok())(i)?;
        class = Some(class1);
        let (i, _) = space0(i)?;
        let (i, comma) = opt(char(','))(i)?;
        input = i;
        if comma.is_some() {
            let (i, _) = space0(i)?;
            let (i, negation) = opt(char('!'))(i)?;
            let (i, attr1) = map_opt(digit1, |s: &str| s.parse::<u32>().ok())(i)?;
            if negation.is_none() {
                attribute = Some(attr1);
            }
            input = i;
        }
    }
    let (i, _) = space0(input)?;
    if !i.is_empty() {
        return Err(nom7::Err::Error(make_error(i, ErrorKind::NonEmpty)));
    }
    return Ok((
        i,
        DetectCipServiceData {
            service,
            class,
            attribute,
        },
    ));
}

fn enip_cip_has_attribute(cipdir: &CipDir, attr: u32) -> std::os::raw::c_int {
    if let CipDir::Request(req) = cipdir {
        for seg in req.path.iter() {
            if seg.segment_type >> 2 == 12 && seg.value == attr {
                return 1;
            }
        }
        match &req.payload {
            EnipCipRequestPayload::GetAttributeList(ga) => {
                for attrg in ga.attr_list.iter() {
                    if attr == u32::from(*attrg) {
                        return 1;
                    }
                }
            }
            EnipCipRequestPayload::SetAttributeList(sa) => {
                if let Some(val) = sa.first_attr {
                    if attr == u32::from(val) {
                        return 1;
                    }
                }
            }
            _ => {}
        }
    }
    return 0;
}

fn enip_cip_has_class(cipdir: &CipDir, class: u32) -> bool {
    if let CipDir::Request(req) = cipdir {
        for seg in req.path.iter() {
            if seg.segment_type >> 2 == 8 && seg.value == class {
                return true;
            }
        }
    }
    return false;
}

fn enip_cip_match_service(d: &CipData, ctx: &DetectCipServiceData) -> std::os::raw::c_int {
    if d.service == ctx.service {
        if let Some(class) = ctx.class {
            if enip_cip_has_class(&d.cipdir, class) {
                if let Some(attr) = ctx.attribute {
                    return enip_cip_has_attribute(&d.cipdir, attr);
                } //else
                return 1;
            } //else
            return 0;
        } //else
        return 1;
    } else if d.service == CIP_MULTIPLE_SERVICE {
        match &d.cipdir {
            CipDir::Request(req) => {
                if let EnipCipRequestPayload::Multiple(m) = &req.payload {
                    for p in m.packet_list.iter() {
                        if enip_cip_match_service(p, ctx) == 1 {
                            return 1;
                        }
                    }
                }
            }
            CipDir::Response(resp) => {
                if let EnipCipResponsePayload::Multiple(m) = &resp.payload {
                    for p in m.packet_list.iter() {
                        if enip_cip_match_service(p, ctx) == 1 {
                            return 1;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    return 0;
}

fn enip_tx_has_cip_service(
    tx: &EnipTransaction, direction: Direction, ctx: &DetectCipServiceData,
) -> std::os::raw::c_int {
    let pduo = if direction == Direction::ToServer {
        &tx.request
    } else {
        &tx.response
    };
    if let Some(pdu) = pduo {
        if let EnipPayload::Cip(c) = &pdu.payload {
            for item in c.items.iter() {
                if let EnipItemPayload::Data(d) = &item.payload {
                    return enip_cip_match_service(&d.cip, ctx);
                }
            }
        }
    }
    return 0;
}

fn enip_cip_match_status(d: &CipData, ctx: &DetectUintData<u8>) -> std::os::raw::c_int {
    if let CipDir::Response(resp) = &d.cipdir {
        if detect_match_uint(ctx, resp.status) {
            return 1;
        }
        if let EnipCipResponsePayload::Multiple(m) = &resp.payload {
            for p in m.packet_list.iter() {
                if enip_cip_match_status(p, ctx) == 1 {
                    return 1;
                }
            }
        }
    }
    return 0;
}

fn enip_tx_has_cip_status(tx: &EnipTransaction, ctx: &DetectUintData<u8>) -> std::os::raw::c_int {
    if let Some(pdu) = &tx.response {
        if let EnipPayload::Cip(c) = &pdu.payload {
            for item in c.items.iter() {
                if let EnipItemPayload::Data(d) = &item.payload {
                    return enip_cip_match_status(&d.cip, ctx);
                }
            }
        }
    }
    return 0;
}

fn enip_cip_match_extendedstatus(d: &CipData, ctx: &DetectUintData<u16>) -> std::os::raw::c_int {
    if let CipDir::Response(resp) = &d.cipdir {
        if resp.status_extended.len() == 2 {
            let val = ((resp.status_extended[1] as u16) << 8) | (resp.status_extended[0] as u16);
            if detect_match_uint(ctx, val) {
                return 1;
            }
        }
        if let EnipCipResponsePayload::Multiple(m) = &resp.payload {
            for p in m.packet_list.iter() {
                if enip_cip_match_extendedstatus(p, ctx) == 1 {
                    return 1;
                }
            }
        }
    }
    return 0;
}

fn enip_tx_has_cip_extendedstatus(
    tx: &EnipTransaction, ctx: &DetectUintData<u16>,
) -> std::os::raw::c_int {
    if let Some(pdu) = &tx.response {
        if let EnipPayload::Cip(c) = &pdu.payload {
            for item in c.items.iter() {
                if let EnipItemPayload::Data(d) = &item.payload {
                    return enip_cip_match_extendedstatus(&d.cip, ctx);
                }
            }
        }
    }
    return 0;
}

fn enip_get_status(tx: &EnipTransaction, direction: Direction) -> Option<u32> {
    if direction == Direction::ToServer {
        if let Some(req) = &tx.request {
            return Some(req.header.status);
        }
    } else if let Some(resp) = &tx.response {
        return Some(resp.header.status);
    }
    return None;
}

fn enip_cip_match_segment(
    d: &CipData, ctx: &DetectUintData<u32>, segment_type: u8,
) -> std::os::raw::c_int {
    if let CipDir::Request(req) = &d.cipdir {
        for seg in req.path.iter() {
            if seg.segment_type >> 2 == segment_type && detect_match_uint(ctx, seg.value) {
                return 1;
            }
        }
        if let EnipCipRequestPayload::Multiple(m) = &req.payload {
            for p in m.packet_list.iter() {
                if enip_cip_match_segment(p, ctx, segment_type) == 1 {
                    return 1;
                }
            }
        }
    }
    return 0;
}

fn enip_tx_has_cip_segment(
    tx: &EnipTransaction, ctx: &DetectUintData<u32>, segment_type: u8,
) -> std::os::raw::c_int {
    if let Some(pdu) = &tx.request {
        if let EnipPayload::Cip(c) = &pdu.payload {
            for item in c.items.iter() {
                if let EnipItemPayload::Data(d) = &item.payload {
                    return enip_cip_match_segment(&d.cip, ctx, segment_type);
                }
            }
        }
    }
    return 0;
}

fn enip_cip_match_attribute(d: &CipData, ctx: &DetectUintData<u32>) -> std::os::raw::c_int {
    if let CipDir::Request(req) = &d.cipdir {
        for seg in req.path.iter() {
            if seg.segment_type >> 2 == 12 && detect_match_uint(ctx, seg.value) {
                return 1;
            }
        }
        match &req.payload {
            EnipCipRequestPayload::GetAttributeList(ga) => {
                for attrg in ga.attr_list.iter() {
                    if detect_match_uint(ctx, (*attrg).into()) {
                        return 1;
                    }
                }
            }
            EnipCipRequestPayload::SetAttributeList(sa) => {
                if let Some(val) = sa.first_attr {
                    if detect_match_uint(ctx, val.into()) {
                        return 1;
                    }
                }
            }
            EnipCipRequestPayload::Multiple(m) => {
                for p in m.packet_list.iter() {
                    if enip_cip_match_attribute(p, ctx) == 1 {
                        return 1;
                    }
                }
            }
            _ => {}
        }
    }
    return 0;
}

fn enip_tx_has_cip_attribute(
    tx: &EnipTransaction, ctx: &DetectUintData<u32>,
) -> std::os::raw::c_int {
    if let Some(pdu) = &tx.request {
        if let EnipPayload::Cip(c) = &pdu.payload {
            for item in c.items.iter() {
                if let EnipItemPayload::Data(d) = &item.payload {
                    return enip_cip_match_attribute(&d.cip, ctx);
                }
            }
        }
    }
    return 0;
}

fn tx_get_protocol_version(tx: &EnipTransaction, direction: Direction) -> Option<u16> {
    if direction == Direction::ToServer {
        if let Some(req) = &tx.request {
            if let EnipPayload::RegisterSession(rs) = &req.payload {
                return Some(rs.protocol_version);
            }
        }
    } else if let Some(resp) = &tx.response {
        match &resp.payload {
            EnipPayload::RegisterSession(rs) => {
                return Some(rs.protocol_version);
            }
            EnipPayload::ListServices(lsp) if !lsp.is_empty() => {
                if let EnipItemPayload::Services(ls) = &lsp[0].payload {
                    return Some(ls.protocol_version);
                }
            }
            EnipPayload::ListIdentity(lip) if !lip.is_empty() => {
                if let EnipItemPayload::Identity(li) = &lip[0].payload {
                    return Some(li.protocol_version);
                }
            }
            _ => {}
        }
    }
    return None;
}

static mut G_ENIP_CIPSERVICE_KW_ID: u16 = 0;
static mut G_ENIP_CIPSERVICE_BUFFER_ID: c_int = 0;
static mut G_ENIP_CAPABILITIES_KW_ID: u16 = 0;
static mut G_ENIP_CAPABILITIES_BUFFER_ID: c_int = 0;
static mut G_ENIP_CIP_ATTRIBUTE_KW_ID: u16 = 0;
static mut G_ENIP_CIP_ATTRIBUTE_BUFFER_ID: c_int = 0;
static mut G_ENIP_CIP_CLASS_KW_ID: u16 = 0;
static mut G_ENIP_CIP_CLASS_BUFFER_ID: c_int = 0;
static mut G_ENIP_VENDOR_ID_KW_ID: u16 = 0;
static mut G_ENIP_VENDOR_ID_BUFFER_ID: c_int = 0;
static mut G_ENIP_STATUS_KW_ID: u16 = 0;
static mut G_ENIP_STATUS_BUFFER_ID: c_int = 0;
static mut G_ENIP_STATE_KW_ID: u16 = 0;
static mut G_ENIP_STATE_BUFFER_ID: c_int = 0;
static mut G_ENIP_SERIAL_KW_ID: u16 = 0;
static mut G_ENIP_SERIAL_BUFFER_ID: c_int = 0;
static mut G_ENIP_REVISION_KW_ID: u16 = 0;
static mut G_ENIP_REVISION_BUFFER_ID: c_int = 0;
static mut G_ENIP_PROTOCOL_VERSION_KW_ID: u16 = 0;
static mut G_ENIP_PROTOCOL_VERSION_BUFFER_ID: c_int = 0;
static mut G_ENIP_PRODUCT_CODE_KW_ID: u16 = 0;
static mut G_ENIP_PRODUCT_CODE_BUFFER_ID: c_int = 0;
static mut G_ENIP_IDENTITY_STATUS_KW_ID: u16 = 0;
static mut G_ENIP_IDENTITY_STATUS_BUFFER_ID: c_int = 0;
static mut G_ENIP_DEVICE_TYPE_KW_ID: u16 = 0;
static mut G_ENIP_DEVICE_TYPE_BUFFER_ID: c_int = 0;
static mut G_ENIP_COMMAND_KW_ID: u16 = 0;
static mut G_ENIP_COMMAND_BUFFER_ID: c_int = 0;
static mut G_ENIP_CIP_STATUS_KW_ID: u16 = 0;
static mut G_ENIP_CIP_STATUS_BUFFER_ID: c_int = 0;
static mut G_ENIP_CIP_INSTANCE_KW_ID: u16 = 0;
static mut G_ENIP_CIP_INSTANCE_BUFFER_ID: c_int = 0;
static mut G_ENIP_CIP_EXTENDEDSTATUS_KW_ID: u16 = 0;
static mut G_ENIP_CIP_EXTENDEDSTATUS_BUFFER_ID: c_int = 0;
static mut G_ENIP_PRODUCT_NAME_BUFFER_ID: c_int = 0;
static mut G_ENIP_SERVICE_NAME_BUFFER_ID: c_int = 0;

unsafe fn parse_cip_service(raw: *const std::os::raw::c_char) -> *mut c_void {
    let raw: &CStr = CStr::from_ptr(raw); //unsafe
    if let Ok(s) = raw.to_str() {
        if let Ok((_, ctx)) = enip_parse_cip_service(s) {
            let boxed = Box::new(ctx);
            return Box::into_raw(boxed) as *mut _;
        }
    }
    return std::ptr::null_mut();
}

unsafe extern "C" fn cipservice_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, raw: *const libc::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    let ctx = parse_cip_service(raw);
    if ctx.is_null() {
        return -1;
    }
    if SCSigMatchAppendSMToList(
        de,
        s,
        G_ENIP_CIPSERVICE_KW_ID,
        ctx as *mut SigMatchCtx,
        G_ENIP_CIPSERVICE_BUFFER_ID,
    )
    .is_null()
    {
        cipservice_free(std::ptr::null_mut(), ctx);
        return -1;
    }
    return 0;
}

unsafe extern "C" fn cipservice_free(_de: *mut DetectEngineCtx, ctx: *mut c_void) {
    std::mem::drop(Box::from_raw(ctx as *mut DetectCipServiceData));
}

unsafe extern "C" fn cipservice_match(
    _de: *mut DetectEngineThreadCtx, _f: *mut Flow, flags: u8, _state: *mut c_void,
    tx: *mut c_void, _sig: *const Signature, ctx: *const SigMatchCtx,
) -> c_int {
    let tx = cast_pointer!(tx, EnipTransaction);
    let ctx = cast_pointer!(ctx, DetectCipServiceData);
    return enip_tx_has_cip_service(tx, flags.into(), ctx);
}

unsafe extern "C" fn capabilities_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, raw: *const libc::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    let ctx = SCDetectU16Parse(raw) as *mut c_void;
    if ctx.is_null() {
        return -1;
    }
    if SCSigMatchAppendSMToList(
        de,
        s,
        G_ENIP_CAPABILITIES_KW_ID,
        ctx as *mut SigMatchCtx,
        G_ENIP_CAPABILITIES_BUFFER_ID,
    )
    .is_null()
    {
        capabilities_free(std::ptr::null_mut(), ctx);
        return -1;
    }
    return 0;
}

fn tx_get_capabilities(tx: &EnipTransaction) -> Option<u16> {
    if let Some(ref response) = tx.response {
        if let EnipPayload::ListServices(lsp) = &response.payload {
            if !lsp.is_empty() {
                if let EnipItemPayload::Services(ls) = &lsp[0].payload {
                    return Some(ls.capabilities);
                }
            }
        }
    }
    return None;
}

unsafe extern "C" fn capabilities_match(
    _de: *mut DetectEngineThreadCtx, _f: *mut Flow, _flags: u8, _state: *mut c_void,
    tx: *mut c_void, _sig: *const Signature, ctx: *const SigMatchCtx,
) -> c_int {
    let tx = cast_pointer!(tx, EnipTransaction);
    let ctx = cast_pointer!(ctx, DetectUintData<u16>);
    if let Some(v) = tx_get_capabilities(tx) {
        return SCDetectU16Match(v, ctx);
    }
    return 0;
}

unsafe extern "C" fn capabilities_free(_de: *mut DetectEngineCtx, ctx: *mut c_void) {
    // Just unbox...
    let ctx = cast_pointer!(ctx, DetectUintData<u16>);
    SCDetectU16Free(ctx);
}

unsafe extern "C" fn cip_attribute_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, raw: *const libc::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    let ctx = SCDetectU32Parse(raw) as *mut c_void;
    if ctx.is_null() {
        return -1;
    }
    if SCSigMatchAppendSMToList(
        de,
        s,
        G_ENIP_CIP_ATTRIBUTE_KW_ID,
        ctx as *mut SigMatchCtx,
        G_ENIP_CIP_ATTRIBUTE_BUFFER_ID,
    )
    .is_null()
    {
        cip_attribute_free(std::ptr::null_mut(), ctx);
        return -1;
    }
    return 0;
}

unsafe extern "C" fn cip_attribute_match(
    _de: *mut DetectEngineThreadCtx, _f: *mut Flow, _flags: u8, _state: *mut c_void,
    tx: *mut c_void, _sig: *const Signature, ctx: *const SigMatchCtx,
) -> c_int {
    let tx = cast_pointer!(tx, EnipTransaction);
    let ctx = cast_pointer!(ctx, DetectUintData<u32>);
    return enip_tx_has_cip_attribute(tx, ctx);
}

unsafe extern "C" fn cip_attribute_free(_de: *mut DetectEngineCtx, ctx: *mut c_void) {
    // Just unbox...
    let ctx = cast_pointer!(ctx, DetectUintData<u32>);
    SCDetectU32Free(ctx);
}

unsafe extern "C" fn cip_class_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, raw: *const libc::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    let ctx = SCDetectU32Parse(raw) as *mut c_void;
    if ctx.is_null() {
        return -1;
    }
    if SCSigMatchAppendSMToList(
        de,
        s,
        G_ENIP_CIP_CLASS_KW_ID,
        ctx as *mut SigMatchCtx,
        G_ENIP_CIP_CLASS_BUFFER_ID,
    )
    .is_null()
    {
        cip_class_free(std::ptr::null_mut(), ctx);
        return -1;
    }
    return 0;
}

unsafe extern "C" fn cip_class_match(
    _de: *mut DetectEngineThreadCtx, _f: *mut Flow, _flags: u8, _state: *mut c_void,
    tx: *mut c_void, _sig: *const Signature, ctx: *const SigMatchCtx,
) -> c_int {
    let tx = cast_pointer!(tx, EnipTransaction);
    let ctx = cast_pointer!(ctx, DetectUintData<u32>);
    return enip_tx_has_cip_segment(tx, ctx, 8);
}

unsafe extern "C" fn cip_class_free(_de: *mut DetectEngineCtx, ctx: *mut c_void) {
    // Just unbox...
    let ctx = cast_pointer!(ctx, DetectUintData<u32>);
    SCDetectU32Free(ctx);
}

unsafe extern "C" fn vendor_id_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, raw: *const libc::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    let ctx = SCDetectU16Parse(raw) as *mut c_void;
    if ctx.is_null() {
        return -1;
    }
    if SCSigMatchAppendSMToList(
        de,
        s,
        G_ENIP_VENDOR_ID_KW_ID,
        ctx as *mut SigMatchCtx,
        G_ENIP_VENDOR_ID_BUFFER_ID,
    )
    .is_null()
    {
        vendor_id_free(std::ptr::null_mut(), ctx);
        return -1;
    }
    return 0;
}

fn tx_get_vendor_id(tx: &EnipTransaction) -> Option<u16> {
    if let Some(ref response) = tx.response {
        if let EnipPayload::ListIdentity(lip) = &response.payload {
            if !lip.is_empty() {
                if let EnipItemPayload::Identity(li) = &lip[0].payload {
                    return Some(li.vendor_id);
                }
            }
        }
    }
    return None;
}

unsafe extern "C" fn vendor_id_match(
    _de: *mut DetectEngineThreadCtx, _f: *mut Flow, _flags: u8, _state: *mut c_void,
    tx: *mut c_void, _sig: *const Signature, ctx: *const SigMatchCtx,
) -> c_int {
    let tx = cast_pointer!(tx, EnipTransaction);
    let ctx = cast_pointer!(ctx, DetectUintData<u16>);
    if let Some(val) = tx_get_vendor_id(tx) {
        return SCDetectU16Match(val, ctx);
    }
    return 0;
}

unsafe extern "C" fn vendor_id_free(_de: *mut DetectEngineCtx, ctx: *mut c_void) {
    // Just unbox...
    let ctx = cast_pointer!(ctx, DetectUintData<u16>);
    SCDetectU16Free(ctx);
}

unsafe extern "C" fn status_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, raw: *const libc::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    let ctx = parse_status(raw) as *mut c_void;
    if ctx.is_null() {
        return -1;
    }
    if SCSigMatchAppendSMToList(
        de,
        s,
        G_ENIP_STATUS_KW_ID,
        ctx as *mut SigMatchCtx,
        G_ENIP_STATUS_BUFFER_ID,
    )
    .is_null()
    {
        status_free(std::ptr::null_mut(), ctx);
        return -1;
    }
    return 0;
}

unsafe extern "C" fn status_match(
    _de: *mut DetectEngineThreadCtx, _f: *mut Flow, flags: u8, _state: *mut c_void,
    tx: *mut c_void, _sig: *const Signature, ctx: *const SigMatchCtx,
) -> c_int {
    let tx = cast_pointer!(tx, EnipTransaction);
    let ctx = cast_pointer!(ctx, DetectUintData<u32>);
    if let Some(x) = enip_get_status(tx, flags.into()) {
        return SCDetectU32Match(x, ctx);
    }
    return 0;
}

unsafe extern "C" fn status_free(_de: *mut DetectEngineCtx, ctx: *mut c_void) {
    // Just unbox...
    let ctx = cast_pointer!(ctx, DetectUintData<u32>);
    SCDetectU32Free(ctx);
}

unsafe extern "C" fn state_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, raw: *const libc::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    let ctx = SCDetectU8Parse(raw) as *mut c_void;
    if ctx.is_null() {
        return -1;
    }
    if SCSigMatchAppendSMToList(
        de,
        s,
        G_ENIP_STATE_KW_ID,
        ctx as *mut SigMatchCtx,
        G_ENIP_STATE_BUFFER_ID,
    )
    .is_null()
    {
        state_free(std::ptr::null_mut(), ctx);
        return -1;
    }
    return 0;
}

fn tx_get_state(tx: &EnipTransaction) -> Option<u8> {
    if let Some(ref response) = tx.response {
        if let EnipPayload::ListIdentity(lip) = &response.payload {
            if !lip.is_empty() {
                if let EnipItemPayload::Identity(li) = &lip[0].payload {
                    return Some(li.state);
                }
            }
        }
    }
    return None;
}

unsafe extern "C" fn state_match(
    _de: *mut DetectEngineThreadCtx, _f: *mut Flow, _flags: u8, _state: *mut c_void,
    tx: *mut c_void, _sig: *const Signature, ctx: *const SigMatchCtx,
) -> c_int {
    let tx = cast_pointer!(tx, EnipTransaction);
    let ctx = cast_pointer!(ctx, DetectUintData<u8>);
    if let Some(val) = tx_get_state(tx) {
        return SCDetectU8Match(val, ctx);
    }
    return 0;
}

unsafe extern "C" fn state_free(_de: *mut DetectEngineCtx, ctx: *mut c_void) {
    // Just unbox...
    let ctx = cast_pointer!(ctx, DetectUintData<u8>);
    SCDetectU8Free(ctx);
}

unsafe extern "C" fn serial_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, raw: *const libc::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    let ctx = SCDetectU32Parse(raw) as *mut c_void;
    if ctx.is_null() {
        return -1;
    }
    if SCSigMatchAppendSMToList(
        de,
        s,
        G_ENIP_SERIAL_KW_ID,
        ctx as *mut SigMatchCtx,
        G_ENIP_SERIAL_BUFFER_ID,
    )
    .is_null()
    {
        serial_free(std::ptr::null_mut(), ctx);
        return -1;
    }
    return 0;
}

fn tx_get_serial(tx: &EnipTransaction) -> Option<u32> {
    if let Some(ref response) = tx.response {
        if let EnipPayload::ListIdentity(lip) = &response.payload {
            if !lip.is_empty() {
                if let EnipItemPayload::Identity(li) = &lip[0].payload {
                    return Some(li.serial);
                }
            }
        }
    }
    return None;
}

unsafe extern "C" fn serial_match(
    _de: *mut DetectEngineThreadCtx, _f: *mut Flow, _flags: u8, _state: *mut c_void,
    tx: *mut c_void, _sig: *const Signature, ctx: *const SigMatchCtx,
) -> c_int {
    let tx = cast_pointer!(tx, EnipTransaction);
    let ctx = cast_pointer!(ctx, DetectUintData<u32>);
    if let Some(val) = tx_get_serial(tx) {
        return SCDetectU32Match(val, ctx);
    }
    return 0;
}

unsafe extern "C" fn serial_free(_de: *mut DetectEngineCtx, ctx: *mut c_void) {
    // Just unbox...
    let ctx = cast_pointer!(ctx, DetectUintData<u32>);
    SCDetectU32Free(ctx);
}

unsafe extern "C" fn revision_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, raw: *const libc::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    let ctx = SCDetectU16Parse(raw) as *mut c_void;
    if ctx.is_null() {
        return -1;
    }
    if SCSigMatchAppendSMToList(
        de,
        s,
        G_ENIP_REVISION_KW_ID,
        ctx as *mut SigMatchCtx,
        G_ENIP_REVISION_BUFFER_ID,
    )
    .is_null()
    {
        revision_free(std::ptr::null_mut(), ctx);
        return -1;
    }
    return 0;
}

fn tx_get_revision(tx: &EnipTransaction) -> Option<u16> {
    if let Some(ref response) = tx.response {
        if let EnipPayload::ListIdentity(lip) = &response.payload {
            if !lip.is_empty() {
                if let EnipItemPayload::Identity(li) = &lip[0].payload {
                    return Some(((li.revision_major as u16) << 8) | (li.revision_minor as u16));
                }
            }
        }
    }
    return None;
}

unsafe extern "C" fn revision_match(
    _de: *mut DetectEngineThreadCtx, _f: *mut Flow, _flags: u8, _state: *mut c_void,
    tx: *mut c_void, _sig: *const Signature, ctx: *const SigMatchCtx,
) -> c_int {
    let tx = cast_pointer!(tx, EnipTransaction);
    let ctx = cast_pointer!(ctx, DetectUintData<u16>);
    if let Some(val) = tx_get_revision(tx) {
        return SCDetectU16Match(val, ctx);
    }
    return 0;
}

unsafe extern "C" fn revision_free(_de: *mut DetectEngineCtx, ctx: *mut c_void) {
    // Just unbox...
    let ctx = cast_pointer!(ctx, DetectUintData<u16>);
    SCDetectU16Free(ctx);
}

unsafe extern "C" fn protocol_version_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, raw: *const libc::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    let ctx = SCDetectU16Parse(raw) as *mut c_void;
    if ctx.is_null() {
        return -1;
    }
    if SCSigMatchAppendSMToList(
        de,
        s,
        G_ENIP_PROTOCOL_VERSION_KW_ID,
        ctx as *mut SigMatchCtx,
        G_ENIP_PROTOCOL_VERSION_BUFFER_ID,
    )
    .is_null()
    {
        protocol_version_free(std::ptr::null_mut(), ctx);
        return -1;
    }
    return 0;
}

unsafe extern "C" fn protocol_version_match(
    _de: *mut DetectEngineThreadCtx, _f: *mut Flow, flags: u8, _state: *mut c_void,
    tx: *mut c_void, _sig: *const Signature, ctx: *const SigMatchCtx,
) -> c_int {
    let tx = cast_pointer!(tx, EnipTransaction);
    let ctx = cast_pointer!(ctx, DetectUintData<u16>);
    if let Some(val) = tx_get_protocol_version(tx, flags.into()) {
        return SCDetectU16Match(val, ctx);
    }
    return 0;
}

unsafe extern "C" fn protocol_version_free(_de: *mut DetectEngineCtx, ctx: *mut c_void) {
    // Just unbox...
    let ctx = cast_pointer!(ctx, DetectUintData<u16>);
    SCDetectU16Free(ctx);
}

unsafe extern "C" fn product_code_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, raw: *const libc::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    let ctx = SCDetectU16Parse(raw) as *mut c_void;
    if ctx.is_null() {
        return -1;
    }
    if SCSigMatchAppendSMToList(
        de,
        s,
        G_ENIP_PRODUCT_CODE_KW_ID,
        ctx as *mut SigMatchCtx,
        G_ENIP_PRODUCT_CODE_BUFFER_ID,
    )
    .is_null()
    {
        product_code_free(std::ptr::null_mut(), ctx);
        return -1;
    }
    return 0;
}

fn tx_get_product_code(tx: &EnipTransaction) -> Option<u16> {
    if let Some(ref response) = tx.response {
        if let EnipPayload::ListIdentity(lip) = &response.payload {
            if !lip.is_empty() {
                if let EnipItemPayload::Identity(li) = &lip[0].payload {
                    return Some(li.product_code);
                }
            }
        }
    }
    return None;
}

unsafe extern "C" fn product_code_match(
    _de: *mut DetectEngineThreadCtx, _f: *mut Flow, _flags: u8, _state: *mut c_void,
    tx: *mut c_void, _sig: *const Signature, ctx: *const SigMatchCtx,
) -> c_int {
    let tx = cast_pointer!(tx, EnipTransaction);
    let ctx = cast_pointer!(ctx, DetectUintData<u16>);
    if let Some(v) = tx_get_product_code(tx) {
        return SCDetectU16Match(v, ctx);
    }
    return 0;
}

unsafe extern "C" fn product_code_free(_de: *mut DetectEngineCtx, ctx: *mut c_void) {
    // Just unbox...
    let ctx = cast_pointer!(ctx, DetectUintData<u16>);
    SCDetectU16Free(ctx);
}

unsafe extern "C" fn identity_status_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, raw: *const libc::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    let ctx = SCDetectU16Parse(raw) as *mut c_void;
    if ctx.is_null() {
        return -1;
    }
    if SCSigMatchAppendSMToList(
        de,
        s,
        G_ENIP_IDENTITY_STATUS_KW_ID,
        ctx as *mut SigMatchCtx,
        G_ENIP_IDENTITY_STATUS_BUFFER_ID,
    )
    .is_null()
    {
        identity_status_free(std::ptr::null_mut(), ctx);
        return -1;
    }
    return 0;
}

fn tx_get_identity_status(tx: &EnipTransaction) -> Option<u16> {
    if let Some(ref response) = tx.response {
        if let EnipPayload::ListIdentity(lip) = &response.payload {
            if !lip.is_empty() {
                if let EnipItemPayload::Identity(li) = &lip[0].payload {
                    return Some(li.status);
                }
            }
        }
    }
    return None;
}

unsafe extern "C" fn identity_status_match(
    _de: *mut DetectEngineThreadCtx, _f: *mut Flow, _flags: u8, _state: *mut c_void,
    tx: *mut c_void, _sig: *const Signature, ctx: *const SigMatchCtx,
) -> c_int {
    let tx = cast_pointer!(tx, EnipTransaction);
    let ctx = cast_pointer!(ctx, DetectUintData<u16>);
    if let Some(v) = tx_get_identity_status(tx) {
        return SCDetectU16Match(v, ctx);
    }
    return 0;
}

unsafe extern "C" fn identity_status_free(_de: *mut DetectEngineCtx, ctx: *mut c_void) {
    // Just unbox...
    let ctx = cast_pointer!(ctx, DetectUintData<u16>);
    SCDetectU16Free(ctx);
}

unsafe extern "C" fn device_type_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, raw: *const libc::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    let ctx = SCDetectU16Parse(raw) as *mut c_void;
    if ctx.is_null() {
        return -1;
    }
    if SCSigMatchAppendSMToList(
        de,
        s,
        G_ENIP_DEVICE_TYPE_KW_ID,
        ctx as *mut SigMatchCtx,
        G_ENIP_DEVICE_TYPE_BUFFER_ID,
    )
    .is_null()
    {
        device_type_free(std::ptr::null_mut(), ctx);
        return -1;
    }
    return 0;
}

fn tx_get_device_type(tx: &EnipTransaction) -> Option<u16> {
    if let Some(ref response) = tx.response {
        if let EnipPayload::ListIdentity(lip) = &response.payload {
            if !lip.is_empty() {
                if let EnipItemPayload::Identity(li) = &lip[0].payload {
                    return Some(li.device_type);
                }
            }
        }
    }
    return None;
}

unsafe extern "C" fn device_type_match(
    _de: *mut DetectEngineThreadCtx, _f: *mut Flow, _flags: u8, _state: *mut c_void,
    tx: *mut c_void, _sig: *const Signature, ctx: *const SigMatchCtx,
) -> c_int {
    let tx = cast_pointer!(tx, EnipTransaction);
    let ctx = cast_pointer!(ctx, DetectUintData<u16>);
    if let Some(v) = tx_get_device_type(tx) {
        return SCDetectU16Match(v, ctx);
    }
    return 0;
}

unsafe extern "C" fn device_type_free(_de: *mut DetectEngineCtx, ctx: *mut c_void) {
    // Just unbox...
    let ctx = cast_pointer!(ctx, DetectUintData<u16>);
    SCDetectU16Free(ctx);
}

unsafe extern "C" fn command_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, raw: *const libc::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    let ctx = parse_command(raw) as *mut c_void;
    if ctx.is_null() {
        return -1;
    }
    if SCSigMatchAppendSMToList(
        de,
        s,
        G_ENIP_COMMAND_KW_ID,
        ctx as *mut SigMatchCtx,
        G_ENIP_COMMAND_BUFFER_ID,
    )
    .is_null()
    {
        command_free(std::ptr::null_mut(), ctx);
        return -1;
    }
    return 0;
}

fn tx_get_command(tx: &EnipTransaction, direction: u8) -> Option<u16> {
    let direction: Direction = direction.into();
    if direction == Direction::ToServer {
        if let Some(req) = &tx.request {
            return Some(req.header.cmd);
        }
    } else if let Some(resp) = &tx.response {
        return Some(resp.header.cmd);
    }
    return None;
}

unsafe extern "C" fn command_match(
    _de: *mut DetectEngineThreadCtx, _f: *mut Flow, flags: u8, _state: *mut c_void,
    tx: *mut c_void, _sig: *const Signature, ctx: *const SigMatchCtx,
) -> c_int {
    let tx = cast_pointer!(tx, EnipTransaction);
    let ctx = cast_pointer!(ctx, DetectUintData<u16>);
    if let Some(v) = tx_get_command(tx, flags) {
        return SCDetectU16Match(v, ctx);
    }
    return 0;
}

unsafe extern "C" fn command_free(_de: *mut DetectEngineCtx, ctx: *mut c_void) {
    // Just unbox...
    let ctx = cast_pointer!(ctx, DetectUintData<u16>);
    SCDetectU16Free(ctx);
}

unsafe extern "C" fn cip_status_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, raw: *const libc::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    let ctx = SCDetectU8Parse(raw) as *mut c_void;
    if ctx.is_null() {
        return -1;
    }
    if SCSigMatchAppendSMToList(
        de,
        s,
        G_ENIP_CIP_STATUS_KW_ID,
        ctx as *mut SigMatchCtx,
        G_ENIP_CIP_STATUS_BUFFER_ID,
    )
    .is_null()
    {
        cip_status_free(std::ptr::null_mut(), ctx);
        return -1;
    }
    return 0;
}

unsafe extern "C" fn cip_status_match(
    _de: *mut DetectEngineThreadCtx, _f: *mut Flow, _flags: u8, _state: *mut c_void,
    tx: *mut c_void, _sig: *const Signature, ctx: *const SigMatchCtx,
) -> c_int {
    let tx = cast_pointer!(tx, EnipTransaction);
    let ctx = cast_pointer!(ctx, DetectUintData<u8>);
    return enip_tx_has_cip_status(tx, ctx);
}

unsafe extern "C" fn cip_status_free(_de: *mut DetectEngineCtx, ctx: *mut c_void) {
    // Just unbox...
    let ctx = cast_pointer!(ctx, DetectUintData<u8>);
    SCDetectU8Free(ctx);
}

unsafe extern "C" fn cip_instance_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, raw: *const libc::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    let ctx = SCDetectU32Parse(raw) as *mut c_void;
    if ctx.is_null() {
        return -1;
    }
    if SCSigMatchAppendSMToList(
        de,
        s,
        G_ENIP_CIP_INSTANCE_KW_ID,
        ctx as *mut SigMatchCtx,
        G_ENIP_CIP_INSTANCE_BUFFER_ID,
    )
    .is_null()
    {
        cip_instance_free(std::ptr::null_mut(), ctx);
        return -1;
    }
    return 0;
}

unsafe extern "C" fn cip_instance_match(
    _de: *mut DetectEngineThreadCtx, _f: *mut Flow, _flags: u8, _state: *mut c_void,
    tx: *mut c_void, _sig: *const Signature, ctx: *const SigMatchCtx,
) -> c_int {
    let tx = cast_pointer!(tx, EnipTransaction);
    let ctx = cast_pointer!(ctx, DetectUintData<u32>);
    return enip_tx_has_cip_segment(tx, ctx, 9);
}

unsafe extern "C" fn cip_instance_free(_de: *mut DetectEngineCtx, ctx: *mut c_void) {
    // Just unbox...
    let ctx = cast_pointer!(ctx, DetectUintData<u32>);
    SCDetectU32Free(ctx);
}

unsafe extern "C" fn cip_extendedstatus_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, raw: *const libc::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    let ctx = SCDetectU16Parse(raw) as *mut c_void;
    if ctx.is_null() {
        return -1;
    }
    if SCSigMatchAppendSMToList(
        de,
        s,
        G_ENIP_CIP_EXTENDEDSTATUS_KW_ID,
        ctx as *mut SigMatchCtx,
        G_ENIP_CIP_EXTENDEDSTATUS_BUFFER_ID,
    )
    .is_null()
    {
        cip_extendedstatus_free(std::ptr::null_mut(), ctx);
        return -1;
    }
    return 0;
}

unsafe extern "C" fn cip_extendedstatus_match(
    _de: *mut DetectEngineThreadCtx, _f: *mut Flow, _flags: u8, _state: *mut c_void,
    tx: *mut c_void, _sig: *const Signature, ctx: *const SigMatchCtx,
) -> c_int {
    let tx = cast_pointer!(tx, EnipTransaction);
    let ctx = cast_pointer!(ctx, DetectUintData<u16>);
    return enip_tx_has_cip_extendedstatus(tx, ctx);
}

unsafe extern "C" fn cip_extendedstatus_free(_de: *mut DetectEngineCtx, ctx: *mut c_void) {
    // Just unbox...
    let ctx = cast_pointer!(ctx, DetectUintData<u16>);
    SCDetectU16Free(ctx);
}

pub unsafe extern "C" fn product_name_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, _raw: *const std::os::raw::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    if SCDetectBufferSetActiveList(de, s, G_ENIP_PRODUCT_NAME_BUFFER_ID) < 0 {
        return -1;
    }
    return 0;
}

unsafe extern "C" fn product_name_get_data(
    tx: *const c_void, _flow_flags: u8, buffer: *mut *const u8, buffer_len: *mut u32,
) -> bool {
    let tx = cast_pointer!(tx, EnipTransaction);
    if let Some(ref response) = tx.response {
        if let EnipPayload::ListIdentity(lip) = &response.payload {
            if !lip.is_empty() {
                if let EnipItemPayload::Identity(li) = &lip[0].payload {
                    *buffer = li.product_name.as_ptr();
                    *buffer_len = li.product_name.len() as u32;
                    return true;
                }
            }
        }
    }
    *buffer = std::ptr::null();
    *buffer_len = 0;
    return false;
}

pub unsafe extern "C" fn service_name_setup(
    de: *mut DetectEngineCtx, s: *mut Signature, _raw: *const std::os::raw::c_char,
) -> c_int {
    if SCDetectSignatureSetAppProto(s, ALPROTO_ENIP) != 0 {
        return -1;
    }
    if SCDetectBufferSetActiveList(de, s, G_ENIP_SERVICE_NAME_BUFFER_ID) < 0 {
        return -1;
    }
    return 0;
}

unsafe extern "C" fn service_name_get_data(
    tx: *const c_void, _flow_flags: u8, buffer: *mut *const u8, buffer_len: *mut u32,
) -> bool {
    let tx = cast_pointer!(tx, EnipTransaction);
    if let Some(ref response) = tx.response {
        if let EnipPayload::ListServices(lsp) = &response.payload {
            if !lsp.is_empty() {
                if let EnipItemPayload::Services(ls) = &lsp[0].payload {
                    *buffer = ls.service_name.as_ptr();
                    *buffer_len = ls.service_name.len() as u32;
                    return true;
                }
            }
        }
    }
    *buffer = std::ptr::null();
    *buffer_len = 0;
    return false;
}

#[no_mangle]
pub unsafe extern "C" fn SCDetectEnipRegister() {
    let kw = SCSigTableAppLiteElmt {
        name: b"cip_service\0".as_ptr() as *const libc::c_char,
        desc: b"match on CIP Service, and optionnally class and attribute\0".as_ptr()
            as *const libc::c_char,
        url: b"/rules/enip-keyword.html#cip_service\0".as_ptr() as *const libc::c_char,
        AppLayerTxMatch: Some(cipservice_match),
        Setup: Some(cipservice_setup),
        Free: Some(cipservice_free),
        flags: 0,
    };
    G_ENIP_CIPSERVICE_KW_ID = SCDetectHelperKeywordRegister(&kw);
    G_ENIP_CIPSERVICE_BUFFER_ID = SCDetectHelperBufferRegister(
        b"cip\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
    );
    let kw = SCSigTableAppLiteElmt {
        name: b"enip.capabilities\0".as_ptr() as *const libc::c_char,
        desc: b"rules for detecting EtherNet/IP capabilities\0".as_ptr() as *const libc::c_char,
        url: b"/rules/enip-keyword.html#enip-capabilities\0".as_ptr() as *const libc::c_char,
        AppLayerTxMatch: Some(capabilities_match),
        Setup: Some(capabilities_setup),
        Free: Some(capabilities_free),
        flags: 0,
    };
    G_ENIP_CAPABILITIES_KW_ID = SCDetectHelperKeywordRegister(&kw);
    G_ENIP_CAPABILITIES_BUFFER_ID = SCDetectHelperBufferRegister(
        b"enip.capabilities\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
    );
    let kw = SCSigTableAppLiteElmt {
        name: b"enip.cip_attribute\0".as_ptr() as *const libc::c_char,
        desc: b"rules for detecting EtherNet/IP cip_attribute\0".as_ptr() as *const libc::c_char,
        url: b"/rules/enip-keyword.html#enip-cip-attribute\0".as_ptr() as *const libc::c_char,
        AppLayerTxMatch: Some(cip_attribute_match),
        Setup: Some(cip_attribute_setup),
        Free: Some(cip_attribute_free),
        flags: 0,
    };
    G_ENIP_CIP_ATTRIBUTE_KW_ID = SCDetectHelperKeywordRegister(&kw);
    G_ENIP_CIP_ATTRIBUTE_BUFFER_ID = SCDetectHelperBufferRegister(
        b"enip.cip_attribute\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
    );
    let kw = SCSigTableAppLiteElmt {
        name: b"enip.cip_class\0".as_ptr() as *const libc::c_char,
        desc: b"rules for detecting EtherNet/IP cip_class\0".as_ptr() as *const libc::c_char,
        url: b"/rules/enip-keyword.html#enip-cip-class\0".as_ptr() as *const libc::c_char,
        AppLayerTxMatch: Some(cip_class_match),
        Setup: Some(cip_class_setup),
        Free: Some(cip_class_free),
        flags: 0,
    };
    G_ENIP_CIP_CLASS_KW_ID = SCDetectHelperKeywordRegister(&kw);
    G_ENIP_CIP_CLASS_BUFFER_ID = SCDetectHelperBufferRegister(
        b"enip.cip_class\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
    );
    let kw = SCSigTableAppLiteElmt {
        name: b"enip.vendor_id\0".as_ptr() as *const libc::c_char,
        desc: b"rules for detecting EtherNet/IP vendor_id\0".as_ptr() as *const libc::c_char,
        url: b"/rules/enip-keyword.html#enip-vendor-id\0".as_ptr() as *const libc::c_char,
        AppLayerTxMatch: Some(vendor_id_match),
        Setup: Some(vendor_id_setup),
        Free: Some(vendor_id_free),
        flags: 0,
    };
    G_ENIP_VENDOR_ID_KW_ID = SCDetectHelperKeywordRegister(&kw);
    G_ENIP_VENDOR_ID_BUFFER_ID = SCDetectHelperBufferRegister(
        b"enip.vendor_id\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
    );
    let kw = SCSigTableAppLiteElmt {
        name: b"enip.status\0".as_ptr() as *const libc::c_char,
        desc: b"rules for detecting EtherNet/IP status\0".as_ptr() as *const libc::c_char,
        url: b"/rules/enip-keyword.html#enip-status\0".as_ptr() as *const libc::c_char,
        AppLayerTxMatch: Some(status_match),
        Setup: Some(status_setup),
        Free: Some(status_free),
        flags: 0,
    };
    G_ENIP_STATUS_KW_ID = SCDetectHelperKeywordRegister(&kw);
    G_ENIP_STATUS_BUFFER_ID = SCDetectHelperBufferRegister(
        b"enip.status\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
    );
    let kw = SCSigTableAppLiteElmt {
        name: b"enip.state\0".as_ptr() as *const libc::c_char,
        desc: b"rules for detecting EtherNet/IP state\0".as_ptr() as *const libc::c_char,
        url: b"/rules/enip-keyword.html#enip-state\0".as_ptr() as *const libc::c_char,
        AppLayerTxMatch: Some(state_match),
        Setup: Some(state_setup),
        Free: Some(state_free),
        flags: 0,
    };
    G_ENIP_STATE_KW_ID = SCDetectHelperKeywordRegister(&kw);
    G_ENIP_STATE_BUFFER_ID = SCDetectHelperBufferRegister(
        b"enip.state\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
    );
    let kw = SCSigTableAppLiteElmt {
        name: b"enip.serial\0".as_ptr() as *const libc::c_char,
        desc: b"rules for detecting EtherNet/IP serial\0".as_ptr() as *const libc::c_char,
        url: b"/rules/enip-keyword.html#enip-serial\0".as_ptr() as *const libc::c_char,
        AppLayerTxMatch: Some(serial_match),
        Setup: Some(serial_setup),
        Free: Some(serial_free),
        flags: 0,
    };
    G_ENIP_SERIAL_KW_ID = SCDetectHelperKeywordRegister(&kw);
    G_ENIP_SERIAL_BUFFER_ID = SCDetectHelperBufferRegister(
        b"enip.serial\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
    );
    let kw = SCSigTableAppLiteElmt {
        name: b"enip.revision\0".as_ptr() as *const libc::c_char,
        desc: b"rules for detecting EtherNet/IP revision\0".as_ptr() as *const libc::c_char,
        url: b"/rules/enip-keyword.html#enip-revision\0".as_ptr() as *const libc::c_char,
        AppLayerTxMatch: Some(revision_match),
        Setup: Some(revision_setup),
        Free: Some(revision_free),
        flags: 0,
    };
    G_ENIP_REVISION_KW_ID = SCDetectHelperKeywordRegister(&kw);
    G_ENIP_REVISION_BUFFER_ID = SCDetectHelperBufferRegister(
        b"enip.revision\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
    );
    let kw = SCSigTableAppLiteElmt {
        name: b"enip.protocol_version\0".as_ptr() as *const libc::c_char,
        desc: b"rules for detecting EtherNet/IP protocol_version\0".as_ptr() as *const libc::c_char,
        url: b"/rules/enip-keyword.html#enip-protocol-version\0".as_ptr() as *const libc::c_char,
        AppLayerTxMatch: Some(protocol_version_match),
        Setup: Some(protocol_version_setup),
        Free: Some(protocol_version_free),
        flags: 0,
    };
    G_ENIP_PROTOCOL_VERSION_KW_ID = SCDetectHelperKeywordRegister(&kw);
    G_ENIP_PROTOCOL_VERSION_BUFFER_ID = SCDetectHelperBufferRegister(
        b"enip.protocol_version\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
    );
    let kw = SCSigTableAppLiteElmt {
        name: b"enip.product_code\0".as_ptr() as *const libc::c_char,
        desc: b"rules for detecting EtherNet/IP product_code\0".as_ptr() as *const libc::c_char,
        url: b"/rules/enip-keyword.html#enip-product-code\0".as_ptr() as *const libc::c_char,
        AppLayerTxMatch: Some(product_code_match),
        Setup: Some(product_code_setup),
        Free: Some(product_code_free),
        flags: 0,
    };
    G_ENIP_PRODUCT_CODE_KW_ID = SCDetectHelperKeywordRegister(&kw);
    G_ENIP_PRODUCT_CODE_BUFFER_ID = SCDetectHelperBufferRegister(
        b"enip.product_code\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
    );
    let kw = SCSigTableAppLiteElmt {
        name: b"enip_command\0".as_ptr() as *const libc::c_char,
        desc: b"rules for detecting EtherNet/IP command\0".as_ptr() as *const libc::c_char,
        url: b"/rules/enip-keyword.html#enip_command\0".as_ptr() as *const libc::c_char,
        AppLayerTxMatch: Some(command_match),
        Setup: Some(command_setup),
        Free: Some(command_free),
        flags: 0,
    };
    G_ENIP_COMMAND_KW_ID = SCDetectHelperKeywordRegister(&kw);
    G_ENIP_COMMAND_BUFFER_ID = SCDetectHelperBufferRegister(
        b"enip.command\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
    );
    let kw = SCSigTableAppLiteElmt {
        name: b"enip.identity_status\0".as_ptr() as *const libc::c_char,
        desc: b"rules for detecting EtherNet/IP identity_status\0".as_ptr() as *const libc::c_char,
        url: b"/rules/enip-keyword.html#enip-identity-status\0".as_ptr() as *const libc::c_char,
        AppLayerTxMatch: Some(identity_status_match),
        Setup: Some(identity_status_setup),
        Free: Some(identity_status_free),
        flags: 0,
    };
    G_ENIP_IDENTITY_STATUS_KW_ID = SCDetectHelperKeywordRegister(&kw);
    G_ENIP_IDENTITY_STATUS_BUFFER_ID = SCDetectHelperBufferRegister(
        b"enip.identity_status\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
    );
    let kw = SCSigTableAppLiteElmt {
        name: b"enip.device_type\0".as_ptr() as *const libc::c_char,
        desc: b"rules for detecting EtherNet/IP device_type\0".as_ptr() as *const libc::c_char,
        url: b"/rules/enip-keyword.html#enip-device-type\0".as_ptr() as *const libc::c_char,
        AppLayerTxMatch: Some(device_type_match),
        Setup: Some(device_type_setup),
        Free: Some(device_type_free),
        flags: 0,
    };
    G_ENIP_DEVICE_TYPE_KW_ID = SCDetectHelperKeywordRegister(&kw);
    G_ENIP_DEVICE_TYPE_BUFFER_ID = SCDetectHelperBufferRegister(
        b"enip.device_type\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
    );
    let kw = SCSigTableAppLiteElmt {
        name: b"enip.cip_status\0".as_ptr() as *const libc::c_char,
        desc: b"rules for detecting EtherNet/IP cip_status\0".as_ptr() as *const libc::c_char,
        url: b"/rules/enip-keyword.html#enip-cip-status\0".as_ptr() as *const libc::c_char,
        AppLayerTxMatch: Some(cip_status_match),
        Setup: Some(cip_status_setup),
        Free: Some(cip_status_free),
        flags: 0,
    };
    G_ENIP_CIP_STATUS_KW_ID = SCDetectHelperKeywordRegister(&kw);
    G_ENIP_CIP_STATUS_BUFFER_ID = SCDetectHelperBufferRegister(
        b"enip.cip_status\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
    );
    let kw = SCSigTableAppLiteElmt {
        name: b"enip.cip_instance\0".as_ptr() as *const libc::c_char,
        desc: b"rules for detecting EtherNet/IP cip_instance\0".as_ptr() as *const libc::c_char,
        url: b"/rules/enip-keyword.html#enip-cip-instance\0".as_ptr() as *const libc::c_char,
        AppLayerTxMatch: Some(cip_instance_match),
        Setup: Some(cip_instance_setup),
        Free: Some(cip_instance_free),
        flags: 0,
    };
    G_ENIP_CIP_INSTANCE_KW_ID = SCDetectHelperKeywordRegister(&kw);
    G_ENIP_CIP_INSTANCE_BUFFER_ID = SCDetectHelperBufferRegister(
        b"enip.cip_instance\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
    );
    let kw = SCSigTableAppLiteElmt {
        name: b"enip.cip_extendedstatus\0".as_ptr() as *const libc::c_char,
        desc: b"rules for detecting EtherNet/IP cip_extendedstatus\0".as_ptr()
            as *const libc::c_char,
        url: b"/rules/enip-keyword.html#enip-cip-extendedstatus\0".as_ptr() as *const libc::c_char,
        AppLayerTxMatch: Some(cip_extendedstatus_match),
        Setup: Some(cip_extendedstatus_setup),
        Free: Some(cip_extendedstatus_free),
        flags: 0,
    };
    G_ENIP_CIP_EXTENDEDSTATUS_KW_ID = SCDetectHelperKeywordRegister(&kw);
    G_ENIP_CIP_EXTENDEDSTATUS_BUFFER_ID = SCDetectHelperBufferRegister(
        b"enip.cip_extendedstatus\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
    );
    let kw = SigTableElmtStickyBuffer {
        name: String::from("enip.product_name"),
        desc: String::from("sticky buffer to match EtherNet/IP product name"),
        url: String::from("/rules/enip-keyword.html#enip-product-name"),
        setup: product_name_setup,
    };
    let _g_enip_product_name_kw_id = helper_keyword_register_sticky_buffer(&kw);
    G_ENIP_PRODUCT_NAME_BUFFER_ID = SCDetectHelperBufferMpmRegister(
        b"enip.product_name\0".as_ptr() as *const libc::c_char,
        b"ENIP product name\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
        Some(product_name_get_data),
    );
    let kw = SigTableElmtStickyBuffer {
        name: String::from("enip.service_name"),
        desc: String::from("sticky buffer to match EtherNet/IP service name"),
        url: String::from("/rules/enip-keyword.html#enip-service-name"),
        setup: service_name_setup,
    };
    let _g_enip_service_name_kw_id = helper_keyword_register_sticky_buffer(&kw);
    G_ENIP_SERVICE_NAME_BUFFER_ID = SCDetectHelperBufferMpmRegister(
        b"enip.service_name\0".as_ptr() as *const libc::c_char,
        b"ENIP service name\0".as_ptr() as *const libc::c_char,
        ALPROTO_ENIP,
        STREAM_TOSERVER | STREAM_TOCLIENT,
        Some(service_name_get_data),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simple test of some valid data.
    #[test]
    fn test_enip_parse_cip_service() {
        let buf1 = "12";
        let (remainder, csd) = enip_parse_cip_service(buf1).unwrap();
        // Check the first message.
        assert_eq!(csd.service, 12);
        assert_eq!(csd.class, None);
        assert_eq!(remainder.len(), 0);

        // with spaces and all values
        let buf2 = "12 , 123 , 45678";
        let (remainder, csd) = enip_parse_cip_service(buf2).unwrap();
        // Check the first message.
        assert_eq!(csd.service, 12);
        assert_eq!(csd.class, Some(123));
        assert_eq!(csd.attribute, Some(45678));
        assert_eq!(remainder.len(), 0);

        // too big for service
        let buf3 = "202";
        assert!(enip_parse_cip_service(buf3).is_err());

        // non numerical after comma
        let buf4 = "123,toto";
        assert!(enip_parse_cip_service(buf4).is_err());

        // too many commas/values
        let buf5 = "1,2,3,4";
        assert!(enip_parse_cip_service(buf5).is_err());

        // too many commas/values
        let buf6 = "1,2,!3";
        let (remainder, csd) = enip_parse_cip_service(buf6).unwrap();
        // Check the first message.
        assert_eq!(csd.service, 1);
        assert_eq!(csd.class, Some(2));
        assert_eq!(csd.attribute, None);
        assert_eq!(remainder.len(), 0);
    }
}
