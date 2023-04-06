mod model;
pub use model::*;

use crate::{get_data, get_data_mut, save_data};
use anyhow::{anyhow, bail, Context, Result};
use arc_swap::ArcSwap;
use once_cell::sync::Lazy;
use prpr::{l10n::LANG_IDENTS, scene::SimpleRecord};
use reqwest::{header, Certificate, Method, RequestBuilder, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{borrow::Cow, collections::HashMap, marker::PhantomData, sync::Arc};

static CERT: Lazy<Certificate> = Lazy::new(|| Certificate::from_pem(include_bytes!("server.crt")).unwrap());

static CLIENT: Lazy<ArcSwap<reqwest::Client>> =
    Lazy::new(|| ArcSwap::from_pointee(reqwest::ClientBuilder::new().add_root_certificate(CERT.clone()).build().unwrap()));

pub struct Client;

// const API_URL: &str = "http://localhost:2924";
const API_URL: &str = "https://api.phira.cn:2925";

fn build_client(access_token: Option<&str>) -> Result<Arc<reqwest::Client>> {
    let mut headers = header::HeaderMap::new();
    headers.append(header::ACCEPT_LANGUAGE, header::HeaderValue::from_str(&get_data().language.clone().unwrap_or(LANG_IDENTS[0].to_string()))?);
    if let Some(token) = access_token {
        let mut auth_value = header::HeaderValue::from_str(&format!("Bearer {}", token))?;
        auth_value.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, auth_value);
    }
    Ok(reqwest::ClientBuilder::new()
        .add_root_certificate(CERT.clone())
        .default_headers(headers)
        .build()?
        .into())
}

pub fn set_access_token_sync(access_token: Option<&str>) -> Result<()> {
    CLIENT.store(build_client(access_token)?);
    Ok(())
}

async fn set_access_token(access_token: &str) -> Result<()> {
    CLIENT.store(build_client(Some(access_token))?);
    Ok(())
}

pub async fn recv_raw(request: RequestBuilder) -> Result<Response> {
    let response = request.send().await?;
    if !response.status().is_success() {
        let text = response.text().await.context("failed to receive text")?;
        if let Ok(what) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(detail) = what["detail"].as_str() {
                bail!("request failed: {detail}");
            }
        }
        bail!("request failed: {text}");
    }
    Ok(response)
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum LoginParams<'a> {
    Password {
        email: &'a str,
        password: &'a str,
    },
    RefreshToken {
        #[serde(rename = "refreshToken")]
        token: &'a str,
    },
}

impl Client {
    #[inline]
    pub fn get(path: impl AsRef<str>) -> RequestBuilder {
        Self::request(Method::GET, path)
    }

    #[inline]
    pub fn post<T: Serialize>(path: impl AsRef<str>, data: &T) -> RequestBuilder {
        Self::request(Method::POST, path).json(data)
    }

    pub fn request(method: Method, path: impl AsRef<str>) -> RequestBuilder {
        CLIENT.load().request(method, API_URL.to_string() + path.as_ref())
    }

    pub async fn load<T: Object + 'static>(id: i32) -> Result<Arc<T>> {
        {
            let map = obtain_map_cache::<T>();
            let mut guard = map.lock().unwrap();
            let Some(actual_map) = guard.downcast_mut::<ObjectMap::<T>>() else { unreachable!() };
            if let Some(value) = actual_map.get(&id) {
                return Ok(Arc::clone(value));
            }
            drop(guard);
            drop(map);
        }
        Self::fetch(id).await
    }

    pub async fn fetch<T: Object + 'static>(id: i32) -> Result<Arc<T>> {
        let value = Arc::new(Client::fetch_inner::<T>(id).await?.ok_or_else(|| anyhow!("entry not found"))?);
        let map = obtain_map_cache::<T>();
        let mut guard = map.lock().unwrap();
        let Some(actual_map) = guard.downcast_mut::<ObjectMap::<T>>() else {
            unreachable!()
        };
        Ok(Arc::clone(actual_map.get_or_insert(id, || value)))
    }

    pub async fn cache_objects<T: Object + 'static>(objects: Vec<T>) -> Result<()> {
        let map = obtain_map_cache::<T>();
        let mut guard = map.lock().unwrap();
        let Some(actual_map) = guard.downcast_mut::<ObjectMap::<T>>() else {
            unreachable!()
        };
        for obj in objects {
            actual_map.put(obj.id(), Arc::new(obj));
        }
        Ok(())
    }

    async fn fetch_inner<T: Object>(id: i32) -> Result<Option<T>> {
        Ok(recv_raw(Self::get(format!("/{}/{id}", T::QUERY_PATH))).await?.json().await?)
    }

    pub fn query<T: Object>() -> QueryBuilder<T> {
        QueryBuilder {
            queries: HashMap::new(),
            page: None,
            _phantom: PhantomData::default(),
        }
    }

    pub async fn register(email: &str, username: &str, password: &str) -> Result<()> {
        recv_raw(Self::post(
            "/register",
            &json!({
                "email": email,
                "name": username,
                "password": password,
            }),
        ))
        .await?;
        Ok(())
    }

    pub async fn login<'a>(params: LoginParams<'a>) -> Result<()> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Resp {
            token: String,
            refresh_token: String,
        }
        let resp: Resp = recv_raw(Self::post("/login", &params)).await?.json().await?;

        set_access_token(&resp.token).await?;
        get_data_mut().tokens = Some((resp.token, resp.refresh_token));
        save_data()?;
        Ok(())
    }

    pub async fn get_me() -> Result<User> {
        Ok(recv_raw(Self::get("/me")).await?.json().await?)
    }

    pub async fn best_record(id: i32) -> Result<SimpleRecord> {
        Ok(recv_raw(Self::get(format!("/record/best/{id}"))).await?.json().await?)
    }
}

#[must_use]
pub struct QueryBuilder<T> {
    queries: HashMap<Cow<'static, str>, Cow<'static, str>>,
    page: Option<u64>,
    _phantom: PhantomData<T>,
}

impl<T: Object> QueryBuilder<T> {
    pub fn query(mut self, key: impl Into<Cow<'static, str>>, value: impl Into<Cow<'static, str>>) -> Self {
        self.queries.insert(key.into(), value.into());
        self
    }

    #[inline]
    pub fn order(self, order: impl Into<Cow<'static, str>>) -> Self {
        self.query("order", order)
    }

    pub fn flag(mut self, flag: impl Into<Cow<'static, str>>) -> Self {
        self.queries.insert(flag.into(), "1".into());
        self
    }

    #[inline]
    pub fn page_num(self, page_num: u64) -> Self {
        self.query("page_num", page_num.to_string())
    }

    pub fn page(mut self, page: u64) -> Self {
        self.page = Some(page);
        self
    }

    pub async fn send(mut self) -> Result<(Vec<T>, u64)> {
        self.queries.insert("page".into(), (self.page.unwrap_or(0) + 1).to_string().into());
        #[derive(Deserialize)]
        struct PagedResult<T> {
            count: u64,
            results: Vec<T>,
        }
        let res: PagedResult<T> = recv_raw(Client::get(format!("/{}", T::QUERY_PATH)).query(&self.queries))
            .await?
            .json()
            .await?;
        Ok((res.results, res.count))
    }
}