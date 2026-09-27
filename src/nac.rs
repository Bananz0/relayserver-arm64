
use std::io::Cursor;

use crate::c::{nac_init_rs, nac_key_establishment_rs, nac_sign_rs};
use crate::error::RelayError;
use plist::{Data, Error};
use serde::{Serialize, Deserialize};


#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
struct SessionInfoRequest {
    session_info_request: Data,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct SessionInfoResponse {
    session_info: Data,
}

#[derive(Deserialize)]
struct CertsResponse {
    cert: Data,
}

pub fn plist_to_buf<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    let mut buf: Vec<u8> = Vec::new();
    let writer = Cursor::new(&mut buf);
    plist::to_writer_xml(writer, &value)?;
    Ok(buf)
}

pub async fn generate_validation_data() -> Result<Vec<u8>, RelayError> {
    println!("[NAC] Step 1: Building HTTP client with timeouts...");
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .use_rustls_tls()
        .timeout(std::time::Duration::from_secs(15))
        .connect_timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| {
            eprintln!("[NAC] Error creating HTTP client: {e}");
            e
        })?;

    println!("[NAC] Step 2: Fetching Apple validation certificate...");
    let cert_url = "https://static.ess.apple.com/identity/validation/cert-1.0.plist";
    let key = match client.get(cert_url).send().await {
        Ok(resp) => resp,
        Err(e) => {
            println!("[NAC] HTTPS cert fetch failed ({e}). Falling back to HTTP...");
            client.get("http://static.ess.apple.com/identity/validation/cert-1.0.plist").send().await.map_err(|e| {
                eprintln!("[NAC] Error fetching cert-1.0.plist: {e}");
                e
            })?
        }
    };
    let key_bytes = key.bytes().await.map_err(|e| {
        eprintln!("[NAC] Error reading cert bytes: {e}");
        e
    })?;
    println!("[NAC] Cert plist downloaded ({} bytes). Parsing...", key_bytes.len());
    let response: CertsResponse = plist::from_bytes(&key_bytes).map_err(|e| {
        eprintln!("[NAC] Error parsing cert plist: {e}");
        e
    })?;
    let certs: Vec<u8> = response.cert.into();
    println!("[NAC] Apple certificate extracted ({} bytes).", certs.len());

    println!("[NAC] Step 3: Calling nac_init_rs (communicating with absd)...");
    let mut output_req = vec![];
    let ctx = nac_init_rs(&certs, &mut output_req).map_err(|e| {
        eprintln!("[NAC] nac_init_rs failed: {e:?}");
        e
    })?;
    println!("[NAC] nac_init_rs succeeded! Context: 0x{:x}, Request len: {} bytes", ctx, output_req.len());

    println!("[NAC] Step 4: Serializing SessionInfoRequest...");
    let init = SessionInfoRequest {
        session_info_request: output_req.into()
    };
    let info = plist_to_buf(&init).map_err(|e| {
        eprintln!("[NAC] Failed to serialize SessionInfoRequest: {e}");
        e
    })?;
    println!("[NAC] SessionInfoRequest serialized ({} bytes). Sending to Apple initializeValidation...", info.len());

    let activation = client.post("https://identity.ess.apple.com/WebObjects/TDIdentityService.woa/wa/initializeValidation")
        .header("Content-Type", "application/x-apple-plist")
        .header("User-Agent", "akd/1.0 (Macintosh; OS X 10.15.7; 19H15)")
        .body(info)
        .send().await
        .map_err(|e| {
            eprintln!("[NAC] initializeValidation request failed: {e}");
            e
        })?;

    let status = activation.status();
    println!("[NAC] Apple initializeValidation response status: {}", status);
    let activation_bytes = activation.bytes().await.map_err(|e| {
        eprintln!("[NAC] Error reading initializeValidation response body: {e}");
        e
    })?;
    println!("[NAC] Apple initializeValidation response body: {} bytes", activation_bytes.len());

    let response: SessionInfoResponse = plist::from_bytes(&activation_bytes).map_err(|e| {
        eprintln!("[NAC] Failed to parse SessionInfoResponse plist: {e}");
        eprintln!("[NAC] Response content: {}", String::from_utf8_lossy(&activation_bytes));
        e
    })?;
    let output: Vec<u8> = response.session_info.into();
    println!("[NAC] Step 5: Calling nac_key_establishment_rs (session_info len: {} bytes)...", output.len());
    nac_key_establishment_rs(ctx, &output).map_err(|e| {
        eprintln!("[NAC] nac_key_establishment_rs failed: {e:?}");
        e
    })?;
    println!("[NAC] nac_key_establishment_rs succeeded!");

    println!("[NAC] Step 6: Calling nac_sign_rs...");
    let sig = nac_sign_rs(ctx, &[]).map_err(|e| {
        eprintln!("[NAC] nac_sign_rs failed: {e:?}");
        e
    })?;
    println!("[NAC] Validation data signature generated successfully ({} bytes)!", sig.len());

    Ok(sig)
}