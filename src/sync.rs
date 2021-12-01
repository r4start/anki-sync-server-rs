use crate::{
    envconfig,
    media::{MediaManager, MediaRecordResult, UploadChangesResult, ZipRequest},
    session::SessionManager,
    user::authenticate,
};

use actix_multipart::Multipart;
use actix_web::{get, web, HttpRequest, HttpResponse, Result};
use anki::{
    backend::Backend,
    backend_proto::sync_server_method_request::Method,
    collection::Collection,
    i18n::I18n,
    media::sync::{
        slog::{self, o},
        zip, BufWriter, FinalizeRequest, FinalizeResponse, RecordBatchRequest, SyncBeginResponse,
        SyncBeginResult,
    },
    storage::{card::row_to_card, note::row_to_note, revlog::row_to_revlog_entry},
    sync::http::SyncRequest,
    sync::{
        http::{HostKeyRequest, HostKeyResponse},
        server::{LocalServer, SyncServer},
        Chunk,
    },
    timestamp::TimestampSecs,
    types::Usn,
};
use rusqlite::params;
use std::{cell::Cell, io, sync::Arc};

use crate::session::Session;
use flate2::read::GzDecoder;
use futures_util::{AsyncWriteExt, TryStreamExt as _};
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use serde_json;
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::{collections::HashMap, io::Read};
use urlparse::urlparse;
static OPERATIONS: [&str; 12] = [
    "hostKey",
    "meta",
    "upload",
    "download",
    "applyChanges",
    "start",
    "applyGraves",
    "chunk",
    "applyChunk",
    "sanityCheck2",
    "finish",
    "abort",
];
static MOPERATIONS: [&str; 5] = [
    "begin",
    "mediaChanges",
    "mediaSanity",
    "uploadChanges",
    "downloadFiles",
];
async fn chunk(col: &Collection) -> Chunk {
    let mut chunk = Chunk::default();
    let conn = &col.storage.db;
    let server_usn = col.usn().unwrap().0;
    let mut stmt = conn.prepare(include_str!("get_review.sql")).unwrap();
    let mut rs = stmt.query(params![server_usn]).unwrap();
    while let Some(r) = rs.next().transpose() {
        let rev = row_to_revlog_entry(r.unwrap()).unwrap();
        chunk.revlog.push(rev);
    }
    let sql1 = "update revlog set usn=? where usn=-1";
    conn.execute(sql1, params![server_usn]).unwrap();

    let mut stmt = conn.prepare(include_str!("get_card.sql")).unwrap();
    let mut rs = stmt.query(params![server_usn]).unwrap();
    while let Some(r) = rs.next().transpose() {
        let card = row_to_card(r.unwrap()).unwrap().into();
        chunk.cards.push(card);
    }
    let sql2 = "update cards set usn=? where usn=-1";
    conn.execute(sql2, params![server_usn]).unwrap();

    let mut stmt = conn.prepare(include_str!("get_note.sql")).unwrap();
    let mut rs = stmt.query(params![server_usn]).unwrap();
    while let Some(r) = rs.next().transpose() {
        let note = row_to_note(r.unwrap()).unwrap().into();
        chunk.notes.push(note);
    }
    let sql3 = "update notes set usn=? where usn=-1";
    conn.execute(sql3, params![server_usn]).unwrap();
    chunk.done = true;
    chunk
}
fn gen_hostkey(username: &str) -> String {
    let mut rng = thread_rng();
    let rand_alphnumr: String = (&mut rng)
        .sample_iter(Alphanumeric)
        .take(8)
        .map(char::from)
        .collect();
    let ts_secs = TimestampSecs::now().to_string();
    let val = [username.to_owned(), ts_secs, rand_alphnumr].join(":");
    let digest = md5::compute(val);
    format!("{:x}", digest)
}

async fn operation_hostkey(
    session_manager: web::Data<Mutex<SessionManager>>,
    hkreq: HostKeyRequest,
) -> Result<Option<HostKeyResponse>> {
    if !authenticate(&hkreq) {
        return Ok(None);
    }
    let hkey = gen_hostkey(&hkreq.username);

    let dir = envconfig::env_variables()
        .get("data_root")
        .unwrap()
        .to_owned();
    let user_path = Path::new(&dir).join(&hkreq.username);
    let session = Session::new(&hkreq.username, user_path);
    session_manager.lock().unwrap().save(hkey.clone(), session);

    let hkres = HostKeyResponse { key: hkey };
    Ok(Some(hkres))
}
fn _decode(data: &[u8], compression: Option<&Vec<u8>>) -> Result<Vec<u8>> {
    let d = if let Some(x) = compression {
        let c = String::from_utf8(x.to_vec()).unwrap();
        if c == "1" {
            let mut d = GzDecoder::new(data);
            let mut b = vec![];
            d.read_to_end(&mut b)?;
            b
        } else {
            data.to_vec()
        }
    } else {
        data.to_vec()
    };
    Ok(d)
}
async fn parse_payload(mut payload: Multipart) -> Result<HashMap<String, Vec<u8>>> {
    let mut map = HashMap::new();
    // iterate over multipart stream
    while let Some(mut field) = payload.try_next().await? {
        let content_disposition = field
            .content_disposition()
            .ok_or_else(|| HttpResponse::BadRequest().finish())?;
        let k = content_disposition.get_name().unwrap().to_owned();

        // Field in turn is stream of *Bytes* object
        let mut v = vec![];
        let mut bw = BufWriter::new(&mut v);
        while let Some(chunk) = field.try_next().await? {
            // must receive all chunks
            bw.get_mut().write_all(&chunk).await.unwrap();
        }
        map.insert(k, v);
    }
    Ok(map)
}
/// favicon handler
#[get("/favicon.ico")]
pub async fn favicon() -> Result<HttpResponse> {
    Ok(HttpResponse::Ok().content_type("text/plain").body(""))
}
#[get("/")]
pub async fn welcome() -> Result<HttpResponse> {
    Ok(HttpResponse::Ok()
        .content_type("text/plain")
        .body("Anki Sync Server"))
}
/// \[("paste-7cd381cbfa7a48319fae2333328863d303794b55.jpg", Some("0")),
///  ("paste-a4084c2983a8b7024e8f98aaa8045c41ec29e7bd.jpg", None),
/// ("paste-f650a5de12d857ad0b51ee6afd62f697b4abf9f7.jpg", Some("2"))\]
async fn adopt_media_changes_from_zip(mm: &MediaManager, zip_data: Vec<u8>) -> (usize, i32) {
    let media_dir = &mm.media_folder;
    let _root = slog::Logger::root(slog::Discard, o!());
    let reader = io::Cursor::new(zip_data);
    let mut zip = zip::ZipArchive::new(reader).unwrap();
    let mut meta_file = zip.by_name("_meta").unwrap();
    let mut v = vec![];
    meta_file.read_to_end(&mut v).unwrap();

    let d: Vec<(String, Option<String>)> = serde_json::from_slice(&v).unwrap();

    let mut media_to_remove = vec![];
    let mut media_to_add = vec![];
    let mut fmap = HashMap::new();
    for (fname, o) in d {
        if let Some(zip_name) = o {
            // on ankidroid zip_name is Some("") if
            // media deleted from client
            if zip_name.is_empty() {
                media_to_remove.push(fname);
            } else {
                fmap.insert(zip_name, fname);
            }
        } else {
            // probably zip_name is None if on PC deleted
            media_to_remove.push(fname);
        }
    }

    drop(meta_file);
    let mut usn = mm.last_usn();
    fs::create_dir_all(&media_dir).unwrap();
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).unwrap();
        let name = file.name();

        if name == "_meta" {
            continue;
        }
        let real_name = fmap.get(name).unwrap();

        let mut data = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut data).unwrap();
        //    write zip data to media folder
        usn += 1;
        let add = mm.add_file(real_name, &data, usn).await;

        media_to_add.push(add);
    }
    let processed_count = media_to_add.len() + media_to_remove.len();
    let lastusn = mm.last_usn();
    // db ops add/delete

    if !media_to_remove.is_empty() {
        mm.delete(media_to_remove.as_slice());
    }
    if !media_to_add.is_empty() {
        mm.records_add(media_to_add);
    }
    (processed_count, lastusn)
}
fn map_sync_req(method: &str) -> Option<Method> {
    match method {
        "hostKey" => Some(Method::HostKey),
        "meta" => Some(Method::Meta),
        "applyChanges" => Some(Method::ApplyChanges),
        "start" => Some(Method::Start),
        "applyGraves" => Some(Method::ApplyGraves),
        "chunk" => Some(Method::Chunk),
        "applyChunk" => Some(Method::ApplyChunk),
        "sanityCheck2" => Some(Method::SanityCheck),
        "finish" => Some(Method::Finish),
        "upload" => Some(Method::FullUpload),
        "download" => Some(Method::FullDownload),
        "abort" => Some(Method::Abort),
        _ => None,
    }
}
/// get hkey from client req bytes if there exist;
/// if not ,get skey ,then get session
pub fn get_session(
    session_manager: &web::Data<Mutex<SessionManager>>,
    map: HashMap<String, Vec<u8>>,
) -> (Option<Session>, Option<String>) {
    let hkey = if let Some(hk) = map.get("k") {
        let hkey = String::from_utf8(hk.to_owned()).unwrap();
        Some(hkey)
    } else {
        None
    };

    let s = if let Some(hkey) = &hkey {
        let s = session_manager.lock().unwrap().load(hkey);
        s
        //    http forbidden if seesion is NOne ?
    } else {
        match map.get("sk") {
            Some(skv) => {
                let skey = String::from_utf8(skv.to_owned()).unwrap();

                Some(
                    session_manager
                        .lock()
                        .unwrap()
                        .load_from_skey(&skey)
                        .unwrap(),
                )
            }
            None => None,
        }
    };
    (s, hkey)
}
pub async fn sync_app(
    session_manager: web::Data<Mutex<SessionManager>>,
    server: web::Data<Mutex<Option<LocalServer>>>,
    payload: Multipart,
    req: HttpRequest,
    web::Path((_, name)): web::Path<(String, String)>,
) -> Result<HttpResponse> {
    let method = req.method().as_str();

    let mut map = HashMap::new();
    if method == "GET" {
        let qs = urlparse(req.uri().path_and_query().unwrap().as_str());
        let query = qs.get_parsed_query().unwrap();
        for (k, v) in query {
            map.insert(k, v.join("").as_bytes().to_vec());
        }
    } else {
        //  POST
        map = parse_payload(payload).await?
    };
    let data_frame = map.get("data");
    // not unzip if compression is None ?
    let data = data_frame
        .as_ref()
        .map(|dt| _decode(dt, map.get("c")).unwrap());

    // add session

    let (sn, hkey) = get_session(&session_manager, map);
    let tr = I18n::template_only();

    match name.as_str() {
        // all normal sync url eg chunk..
        op if OPERATIONS.contains(&op) => {
            // create a new server obj

            let mtd = map_sync_req(op);
            let data = if mtd == Some(Method::FullUpload) {
                //   write data from client to file ,as its db data,and return
                // its path in bytes
                let session = sn.clone().unwrap();
                let colpath = format!("{}.tmp", session.get_col_path().display());
                let colp = Path::new(&colpath);
                fs::write(colp, data.unwrap()).unwrap();
                Some(colpath.as_bytes().to_owned())
            } else if mtd == Some(Method::FullDownload) {
                let v: Vec<u8> = Vec::new();
                Some(v)
            } else {
                data
            };

            let syncreq =
                SyncRequest::from_method_and_data(mtd.unwrap(), data.as_ref().unwrap().clone())
                    .unwrap();
            match syncreq {
                SyncRequest::HostKey(x) => {
                    let res = operation_hostkey(session_manager, x).await?;
                    if let Some(resp) = res {
                        Ok(HttpResponse::Ok().json(resp))
                    } else {
                        Ok(HttpResponse::NonAuthoritativeInformation().finish())
                    }
                }
                x => {
                    // session None is forbidden

                    // let mut backend = Backend::new(tr, true);
                    // backend.col = Arc::new(Mutex::new(Some(col)));
                    match x {
                        SyncRequest::ApplyChanges(u) => {
                            // let mut server = backend.col_into_server().unwrap();

                            // server.client_usn = Usn {
                            //     0: sn.clone().unwrap().client_usn,
                            // };
                            // server.client_is_newer = sn.clone().unwrap().client_newer;
                            // server.server_usn = Usn {
                            //     0: sn.clone().unwrap().server_usn,
                            // };
                            println!(
                                "{}",
                                server.lock().unwrap().as_ref().unwrap().client_is_newer
                            );
                            let z = server
                                .lock()
                                .unwrap()
                                .as_mut()
                                .unwrap()
                                .apply_changes(u.changes)
                                .await
                                .unwrap();

                            Ok(HttpResponse::Ok().json(z))
                        }
                        SyncRequest::Meta(x) => {
                         let server=   if server.lock().unwrap().is_some() {
                               if !(&server.lock().unwrap().as_ref().unwrap().col.col_path== &sn.clone().unwrap().get_col_path()){
                                let col = sn.clone().unwrap().get_col();
                                *server.lock().unwrap() = Some(LocalServer::new(col));
                            server  
                            }else{
                                   server
                               }
                            }else{
                                let col = sn.clone().unwrap().get_col();
                                *server.lock().unwrap() = Some(LocalServer::new(col));
                            server
                            };
                            // let col = sn.clone().unwrap().get_col();
                            // *server.lock().unwrap() = Some(LocalServer::new(col));
                            let m = server
                                .lock()
                                .unwrap()
                                .as_mut()
                                .unwrap()
                                .meta()
                                .await
                                .unwrap();
                            Ok(HttpResponse::Ok().json(m))
                        }
                        SyncRequest::Start(x) => {
                            // let mut s = sn.unwrap();
                            // s.client_newer = x.local_is_newer;
                            // s.client_usn = x.client_usn.0;

                            // let mut server = backend.col_into_server().unwrap();
                            // let usn = server.col.usn().unwrap().0;
                            // s.server_usn = usn;
                            // session_manager
                            //     .lock()
                            //     .unwrap()
                            //     .sessions
                            //     .insert(hkey.unwrap(), s);

                            // server.col.storage.begin_trx().unwrap();
                            // let grav = server
                            //     .start(x.client_usn, x.local_is_newer, x.deprecated_client_graves)
                            //     .await
                            //     .unwrap();

                            // server.col.storage.commit_trx().unwrap();
                            // server.into_col().storage.db.close().unwrap();

                            // *server.lock().unwrap()=Some(LocalServer::new(col));

                            let m = server
                                .lock()
                                .unwrap()
                                .as_mut()
                                .unwrap()
                                .start(x.client_usn, x.local_is_newer, x.deprecated_client_graves)
                                .await
                                .unwrap();
                            println!(
                                "start {}{}",
                                x.client_usn,
                                server.lock().unwrap().as_ref().unwrap().server_usn
                            );

                            Ok(HttpResponse::Ok().json(m))
                        }
                        SyncRequest::ApplyGraves(u) => {
                            // let mut server = backend.col_into_server().unwrap();

                            // server.server_usn = Usn {
                            //     0: sn.unwrap().server_usn,
                            // };
                            // server.col.storage.begin_trx().unwrap();
                            // server.apply_graves(u.chunk).await.unwrap();
                            // server.col.storage.commit_trx().unwrap();
                            server
                                .lock()
                                .unwrap()
                                .as_mut()
                                .unwrap()
                                .apply_graves(u.chunk)
                                .await
                                .unwrap();

                            Ok(HttpResponse::Ok().body("null"))
                        }
                        SyncRequest::Chunk => {
                            // let z = backend.col_into_server().unwrap().into_col();
                            // let z = server.lock();
                            // let chunk = chunk(&z.unwrap().as_ref().unwrap().col).await;
                            let chunk = server
                                .lock()
                                .unwrap()
                                .as_mut()
                                .unwrap()
                                .chunk()
                                .await
                                .unwrap();
                            Ok(HttpResponse::Ok().json(chunk))
                        }
                        SyncRequest::ApplyChunk(u) => {
                            // let mut server = backend.col_into_server().unwrap();
                            // server.client_usn = Usn {
                            //     0: sn.clone().unwrap().client_usn,
                            // };
                            // server.client_is_newer = sn.unwrap().client_newer;
                            // server.apply_chunk(u.chunk).await.unwrap();
                            server
                                .lock()
                                .unwrap()
                                .as_mut()
                                .unwrap()
                                .apply_chunk(u.chunk)
                                .await
                                .unwrap();

                            Ok(HttpResponse::Ok().body("null"))
                        }
                        SyncRequest::SanityCheck(u) => {
                            // let z = backend
                            //     .col_into_server()
                            //     .unwrap()
                            //     .sanity_check(u.client)
                            //     .await
                            //     .unwrap();

                            let z = server
                                .lock()
                                .unwrap()
                                .as_mut()
                                .unwrap()
                                .sanity_check(u.client)
                                .await
                                .unwrap();
                            println!("{:?}", &z);
                            Ok(HttpResponse::Ok().json(z))
                        }
                        SyncRequest::Finish => {
                            // let z = backend.col_into_server().unwrap().finish().await.unwrap();

                            let z = server
                                .lock()
                                .unwrap()
                                .as_mut()
                                .unwrap()
                                .finish()
                                .await
                                .unwrap();

                            Ok(HttpResponse::Ok().json(z))
                        }
                        SyncRequest::FullUpload(u) => {
                            // let s = backend.col_into_server().unwrap();

                            // Box::new(server.lock().unwrap().as_mut().unwrap()).full_upload(&u, true).await.unwrap();
                            let mut s = server.lock().unwrap().take().unwrap();
                            s.col.before_upload().unwrap();
                            Box::new(s).full_upload(&u, true).await.unwrap();
                            Ok(HttpResponse::Ok().body("OK"))
                        }
                        SyncRequest::FullDownload => {
                            // let s = backend.col_into_server().unwrap();
                            let s = server.lock().unwrap().take().unwrap();
                            let file = Box::new(s).full_download(None).await.unwrap();
                            let mut file_buffer = vec![];
                            fs::File::open(file)
                                .unwrap()
                                .read_to_end(&mut file_buffer)
                                .unwrap();
                            Ok(HttpResponse::Ok().body(file_buffer))
                        }
                        p => {
                            // let d = backend.sync_server_method_inner(p).unwrap();

                            Ok(HttpResponse::Ok().finish())
                        }
                    }
                }
            }
        }
        // media sync
        media_op if MOPERATIONS.contains(&media_op) => {
            // session None is forbidden
            let session = sn.clone().unwrap();
            let (md, mf) = session.get_md_mf();

            let mm = MediaManager::new(mf, md).unwrap();
            match media_op {
                "begin" => {
                    let lastusn = mm.last_usn();
                    let sbr = SyncBeginResult {
                        data: Some(SyncBeginResponse {
                            sync_key: sn.clone().unwrap().skey(),
                            usn: lastusn,
                        }),
                        err: String::new(),
                    };
                    Ok(HttpResponse::Ok().json(sbr))
                }
                "uploadChanges" => {
                    let (procs_cnt, lastusn) =
                        adopt_media_changes_from_zip(&mm, data.unwrap()).await;

                    //    dererial uploadreslt
                    let upres = UploadChangesResult {
                        data: Some(vec![procs_cnt, lastusn as usize]),
                        err: String::new(),
                    };
                    Ok(HttpResponse::Ok().json(upres))
                }
                "mediaChanges" => {
                    //client lastusn 0
                    // server ls 135
                    // rec1634015317.mp3 135 None
                    // sapi5js-42ecd8a6-427ac916-0ba420b0-b1c11b85-f20d5990.mp3 134 None
                    // paste-c9bde250ab49048b2cfc90232a3ae5402aba19c3.jpg 133 c9bde250ab49048b2cfc90232a3ae5402aba19c3
                    // paste-d8d989d662ae46a420ec5d440516912c5fbf2111.jpg 132 d8d989d662ae46a420ec5d440516912c5fbf2111
                    let rbr: RecordBatchRequest = serde_json::from_slice(&data.unwrap()).unwrap();
                    let client_lastusn = rbr.last_usn;
                    let server_lastusn = mm.last_usn();

                    let d = if client_lastusn < server_lastusn || client_lastusn == 0 {
                        let mut chges = mm.changes(client_lastusn);
                        chges.reverse();
                        MediaRecordResult {
                            data: Some(chges),
                            err: String::new(),
                        }
                    } else {
                        MediaRecordResult {
                            data: Some(Vec::new()),
                            err: String::new(),
                        }
                    };

                    Ok(HttpResponse::Ok().json(d))
                }
                "downloadFiles" => {
                    // client data :requested filenames
                    // "{\"files\":[\"paste-ceaa6863ee1c4ee38ed1cd3a0a2719fa934517ed.jpg\",
                    // \"sapi5js-08c91aeb-d6ae72e4-fa3faf05-eff30d1f-581b71c8.mp3\",
                    // \"sapi5js-2750d034-14d4845f-b60dc87b-afb7197f-87930ab7.mp3\"]}

                    let v: ZipRequest = serde_json::from_slice(&data.unwrap()).unwrap();
                    let d = mm.zip_files(v).unwrap();

                    Ok(HttpResponse::Ok().body(d.unwrap()))
                }
                "mediaSanity" => {
                    let locol: FinalizeRequest =
                        serde_json::from_slice(&data.clone().unwrap()).unwrap();
                    let res = if mm.count() == locol.local {
                        "OK"
                    } else {
                        "FAILED"
                    };
                    let result = FinalizeResponse {
                        data: Some(res.to_owned()),
                        err: String::new(),
                    };
                    Ok(HttpResponse::Ok().json(result))
                }
                _ => Ok(HttpResponse::Ok().finish()),
            }
        }

        _ => Ok(HttpResponse::NotFound().finish()),
    }
}

#[test]
fn test_gen_random() {
    // String:
    let mut rng = thread_rng();
    let s: String = (&mut rng)
        .sample_iter(Alphanumeric)
        .take(8)
        .map(char::from)
        .collect();
    // MD0ZcI2
    println!("{}", &s);
}
#[test]
fn test_tssecs() {
    let ts = TimestampSecs::now();
    // 1634543952
    println!("{}", ts);
}

/// {"v": \["anki,2.1.49 (7a232b70),win:10"],
/// "k": \["0f5c8659ec6771eed\
/// 3b5d473816699e7"]}
#[test]
fn test_parse_qs() {
    let url = urlparse(
        "/msync/begin?k=0f5c8659ec6771eed3b5d473816699e7&v=anki%2C2.1.49+%287a232b70%29%2Cwin%3A10",
    );
    let query = url.get_parsed_query().unwrap();
    println!("{:?}", url);
    println!("{:?}", query);
}
