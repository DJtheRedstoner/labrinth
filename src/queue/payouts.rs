use crate::models::ids::UserId;
use crate::models::payouts::{
    PayoutDecimal, PayoutInterval, PayoutMethod, PayoutMethodFee, PayoutMethodType,
};
use crate::routes::ApiError;
use crate::util::env::parse_var;
use crate::{database::redis::RedisPool, models::projects::MonetizationStatus};
use base64::Engine;
use chrono::{DateTime, Datelike, Duration, Utc, Weekday};
use dashmap::DashMap;
use reqwest::Method;
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::postgres::PgQueryResult;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

pub struct PayoutsQueue {
    credential: RwLock<Option<PayPalCredentials>>,
    payout_options: RwLock<Option<PayoutMethods>>,
    payouts_locks: DashMap<UserId, Arc<Mutex<()>>>,
}

#[derive(Clone)]
struct PayPalCredentials {
    access_token: String,
    token_type: String,
    expires: DateTime<Utc>,
}

#[derive(Clone)]
struct PayoutMethods {
    options: Vec<PayoutMethod>,
    expires: DateTime<Utc>,
}

impl Default for PayoutsQueue {
    fn default() -> Self {
        Self::new()
    }
}
// Batches payouts and handles token refresh
impl PayoutsQueue {
    pub fn new() -> Self {
        PayoutsQueue {
            credential: RwLock::new(None),
            payout_options: RwLock::new(None),
            payouts_locks: DashMap::new(),
        }
    }

    async fn refresh_token(&self) -> Result<PayPalCredentials, ApiError> {
        let mut creds = self.credential.write().await;
        let client = reqwest::Client::new();

        let combined_key = format!(
            "{}:{}",
            dotenvy::var("PAYPAL_CLIENT_ID")?,
            dotenvy::var("PAYPAL_CLIENT_SECRET")?
        );
        let formatted_key = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(combined_key)
        );

        let mut form = HashMap::new();
        form.insert("grant_type", "client_credentials");

        #[derive(Deserialize)]
        struct PaypalCredential {
            access_token: String,
            token_type: String,
            expires_in: i64,
        }

        let credential: PaypalCredential = client
            .post(&format!("{}oauth2/token", dotenvy::var("PAYPAL_API_URL")?))
            .header("Accept", "application/json")
            .header("Accept-Language", "en_US")
            .header("Authorization", formatted_key)
            .form(&form)
            .send()
            .await
            .map_err(|_| ApiError::Payments("Error while authenticating with PayPal".to_string()))?
            .json()
            .await
            .map_err(|_| {
                ApiError::Payments(
                    "Error while authenticating with PayPal (deser error)".to_string(),
                )
            })?;

        let new_creds = PayPalCredentials {
            access_token: credential.access_token,
            token_type: credential.token_type,
            expires: Utc::now() + Duration::seconds(credential.expires_in),
        };

        *creds = Some(new_creds.clone());

        Ok(new_creds)
    }

    pub async fn make_paypal_request<T: Serialize, X: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<T>,
        raw_text: Option<String>,
        no_api_prefix: Option<bool>,
    ) -> Result<X, ApiError> {
        let read = self.credential.read().await;
        let credentials = if let Some(credentials) = read.as_ref() {
            if credentials.expires < Utc::now() {
                drop(read);
                self.refresh_token().await.map_err(|_| {
                    ApiError::Payments("Error while authenticating with PayPal".to_string())
                })?
            } else {
                credentials.clone()
            }
        } else {
            drop(read);
            self.refresh_token().await.map_err(|_| {
                ApiError::Payments("Error while authenticating with PayPal".to_string())
            })?
        };

        let client = reqwest::Client::new();
        let mut request = client
            .request(
                method,
                if no_api_prefix.unwrap_or(false) {
                    path.to_string()
                } else {
                    format!("{}{path}", dotenvy::var("PAYPAL_API_URL")?)
                },
            )
            .header(
                "Authorization",
                format!("{} {}", credentials.token_type, credentials.access_token),
            );

        if let Some(body) = body {
            request = request.json(&body);
        } else if let Some(body) = raw_text {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body);
        }

        let resp = request
            .send()
            .await
            .map_err(|_| ApiError::Payments("could not communicate with PayPal".to_string()))?;

        let status = resp.status();

        let value = resp.json::<Value>().await.map_err(|_| {
            ApiError::Payments("could not retrieve PayPal response body".to_string())
        })?;

        if !status.is_success() {
            #[derive(Deserialize)]
            struct PayPalError {
                pub name: String,
                pub message: String,
            }

            #[derive(Deserialize)]
            struct PayPalIdentityError {
                pub error: String,
                pub error_description: String,
            }

            if let Ok(error) = serde_json::from_value::<PayPalError>(value.clone()) {
                return Err(ApiError::Payments(format!(
                    "error name: {}, message: {}",
                    error.name, error.message
                )));
            }

            if let Ok(error) = serde_json::from_value::<PayPalIdentityError>(value) {
                return Err(ApiError::Payments(format!(
                    "error name: {}, message: {}",
                    error.error, error.error_description
                )));
            }

            return Err(ApiError::Payments(
                "could not retrieve PayPal error body".to_string(),
            ));
        }

        Ok(serde_json::from_value(value)?)
    }

    pub async fn make_tremendous_request<T: Serialize, X: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<T>,
    ) -> Result<X, ApiError> {
        let client = reqwest::Client::new();
        let mut request = client
            .request(
                method,
                format!("{}{path}", dotenvy::var("TREMENDOUS_API_URL")?),
            )
            .header(
                "Authorization",
                format!("Bearer {}", dotenvy::var("TREMENDOUS_API_KEY")?),
            );

        if let Some(body) = body {
            request = request.json(&body);
        }

        let resp = request
            .send()
            .await
            .map_err(|_| ApiError::Payments("could not communicate with Tremendous".to_string()))?;

        let status = resp.status();

        let value = resp.json::<Value>().await.map_err(|_| {
            ApiError::Payments("could not retrieve Tremendous response body".to_string())
        })?;

        if !status.is_success() {
            if let Some(obj) = value.as_object() {
                if let Some(array) = obj.get("errors") {
                    #[derive(Deserialize)]
                    struct TremendousError {
                        message: String,
                    }

                    let err =
                        serde_json::from_value::<TremendousError>(array.clone()).map_err(|_| {
                            ApiError::Payments(
                                "could not retrieve Tremendous error json body".to_string(),
                            )
                        })?;

                    return Err(ApiError::Payments(err.message));
                }

                return Err(ApiError::Payments(
                    "could not retrieve Tremendous error body".to_string(),
                ));
            }
        }

        Ok(serde_json::from_value(value)?)
    }

    pub async fn get_payout_methods(&self) -> Result<Vec<PayoutMethod>, ApiError> {
        async fn refresh_payout_methods(queue: &PayoutsQueue) -> Result<PayoutMethods, ApiError> {
            let mut options = queue.payout_options.write().await;

            let mut methods = Vec::new();

            #[derive(Deserialize)]
            pub struct Sku {
                pub min: Decimal,
                pub max: Decimal,
            }

            #[derive(Deserialize, Eq, PartialEq)]
            #[serde(rename_all = "snake_case")]
            pub enum ProductImageType {
                Card,
                Logo,
            }

            #[derive(Deserialize)]
            pub struct ProductImage {
                pub src: String,
                #[serde(rename = "type")]
                pub type_: ProductImageType,
            }

            #[derive(Deserialize)]
            pub struct ProductCountry {
                pub abbr: String,
            }

            #[derive(Deserialize)]
            pub struct Product {
                pub id: String,
                pub category: String,
                pub name: String,
                pub description: String,
                pub disclosure: String,
                pub skus: Vec<Sku>,
                pub currency_codes: Vec<String>,
                pub countries: Vec<ProductCountry>,
                pub images: Vec<ProductImage>,
            }

            #[derive(Deserialize)]
            pub struct TremendousResponse {
                pub products: Vec<Product>,
            }

            let response = queue
                .make_tremendous_request::<(), TremendousResponse>(Method::GET, "products", None)
                .await?;

            for product in response.products {
                const BLACKLISTED_IDS: &[&str] = &[
                    // physical visa
                    "A2J05SWPI2QG",
                    // crypto
                    "1UOOSHUUYTAM",
                    "5EVJN47HPDFT",
                    "NI9M4EVAVGFJ",
                    "VLY29QHTMNGT",
                    "7XU98H109Y3A",
                    "0CGEDFP2UIKV",
                    "PDYLQU0K073Y",
                    "HCS5Z7O2NV5G",
                    "IY1VMST1MOXS",
                    "VRPZLJ7HCA8X",
                    // bitcard (crypto)
                    "GWQQS5RM8IZS",
                    "896MYD4SGOGZ",
                    "PWLEN1VZGMZA",
                    "A2VRM96J5K5W",
                    "HV9ICIM3JT7P",
                    "K2KLSPVWC2Q4",
                    "HRBRQLLTDF95",
                    "UUBYLZVK7QAB",
                    "BH8W3XEDEOJN",
                    "7WGE043X1RYQ",
                    "2B13MHUZZVTF",
                    "JN6R44P86EYX",
                    "DA8H43GU84SO",
                    "QK2XAQHSDEH4",
                    "J7K1IQFS76DK",
                    "NL4JQ2G7UPRZ",
                    "OEFTMSBA5ELH",
                    "A3CQK6UHNV27",
                ];
                const SUPPORTED_METHODS: &[&str] =
                    &["merchant_cards", "visa", "bank", "ach", "visa_card"];

                if !SUPPORTED_METHODS.contains(&&*product.category)
                    || BLACKLISTED_IDS.contains(&&*product.id)
                {
                    continue;
                };

                let method = PayoutMethod {
                    id: product.id,
                    type_: PayoutMethodType::Tremendous,
                    name: product.name.clone(),
                    supported_countries: product.countries.into_iter().map(|x| x.abbr).collect(),
                    image_url: product
                        .images
                        .into_iter()
                        .find(|x| x.type_ == ProductImageType::Card)
                        .map(|x| x.src),
                    interval: if product.skus.len() > 1 {
                        let mut values = product
                            .skus
                            .into_iter()
                            .map(|x| PayoutDecimal(x.min))
                            .collect::<Vec<_>>();
                        values.sort_by(|a, b| a.0.cmp(&b.0));

                        PayoutInterval::Fixed { values }
                    } else if let Some(first) = product.skus.first() {
                        PayoutInterval::Standard {
                            min: first.min,
                            max: first.max,
                        }
                    } else {
                        PayoutInterval::Standard {
                            min: Decimal::ZERO,
                            max: Decimal::from(5_000),
                        }
                    },
                    fee: if product.category == "ach" {
                        PayoutMethodFee {
                            percentage: Decimal::from(4) / Decimal::from(100),
                            min: Decimal::from(1) / Decimal::from(4),
                            max: None,
                        }
                    } else {
                        PayoutMethodFee {
                            percentage: Default::default(),
                            min: Default::default(),
                            max: None,
                        }
                    },
                };

                // we do not support interval gift cards with non US based currencies since we cannot do currency conversions properly
                if let PayoutInterval::Fixed { .. } = method.interval {
                    if !product.currency_codes.contains(&"USD".to_string()) {
                        continue;
                    }
                }

                methods.push(method);
            }

            const UPRANK_IDS: &[&str] = &["ET0ZVETV5ILN", "Q24BD9EZ332JT", "UIL1ZYJU5MKN"];
            const DOWNRANK_IDS: &[&str] = &["EIPF8Q00EMM1", "OU2MWXYWPNWQ"];

            methods.sort_by(|a, b| {
                let a_top = UPRANK_IDS.contains(&&*a.id);
                let a_bottom = DOWNRANK_IDS.contains(&&*a.id);
                let b_top = UPRANK_IDS.contains(&&*b.id);
                let b_bottom = DOWNRANK_IDS.contains(&&*b.id);

                match (a_top, a_bottom, b_top, b_bottom) {
                    (true, _, true, _) => a.name.cmp(&b.name), // Both in top_priority: sort alphabetically
                    (_, true, _, true) => a.name.cmp(&b.name), // Both in bottom_priority: sort alphabetically
                    (true, _, _, _) => std::cmp::Ordering::Less, // a in top_priority: a comes first
                    (_, _, true, _) => std::cmp::Ordering::Greater, // b in top_priority: b comes first
                    (_, true, _, _) => std::cmp::Ordering::Greater, // a in bottom_priority: b comes first
                    (_, _, _, true) => std::cmp::Ordering::Less, // b in bottom_priority: a comes first
                    (_, _, _, _) => a.name.cmp(&b.name), // Neither in priority: sort alphabetically
                }
            });

            {
                let paypal_us = PayoutMethod {
                    id: "paypal_us".to_string(),
                    type_: PayoutMethodType::PayPal,
                    name: "PayPal".to_string(),
                    supported_countries: vec!["US".to_string()],
                    image_url: None,
                    interval: PayoutInterval::Standard {
                        min: Decimal::from(1) / Decimal::from(4),
                        max: Decimal::from(100_000),
                    },
                    fee: PayoutMethodFee {
                        percentage: Decimal::from(2) / Decimal::from(100),
                        min: Decimal::from(1) / Decimal::from(4),
                        max: Some(Decimal::from(1)),
                    },
                };

                let mut venmo = paypal_us.clone();
                venmo.id = "venmo".to_string();
                venmo.name = "Venmo".to_string();
                venmo.type_ = PayoutMethodType::Venmo;

                methods.insert(0, paypal_us);
                methods.insert(1, venmo)
            }

            methods.insert(
                2,
                PayoutMethod {
                    id: "paypal_in".to_string(),
                    type_: PayoutMethodType::PayPal,
                    name: "PayPal".to_string(),
                    supported_countries: rust_iso3166::ALL
                        .iter()
                        .filter(|x| x.alpha2 != "US")
                        .map(|x| x.alpha2.to_string())
                        .collect(),
                    image_url: None,
                    interval: PayoutInterval::Standard {
                        min: Decimal::from(1) / Decimal::from(4),
                        max: Decimal::from(100_000),
                    },
                    fee: PayoutMethodFee {
                        percentage: Decimal::from(2) / Decimal::from(100),
                        min: Decimal::ZERO,
                        max: Some(Decimal::from(20)),
                    },
                },
            );

            let new_options = PayoutMethods {
                options: methods,
                expires: Utc::now() + Duration::hours(6),
            };

            *options = Some(new_options.clone());

            Ok(new_options)
        }

        let read = self.payout_options.read().await;
        let options = if let Some(options) = read.as_ref() {
            if options.expires < Utc::now() {
                drop(read);
                refresh_payout_methods(self).await?
            } else {
                options.clone()
            }
        } else {
            drop(read);
            refresh_payout_methods(self).await?
        };

        Ok(options.options)
    }

    pub fn lock_user_payouts(&self, user_id: UserId) -> Arc<Mutex<()>> {
        self.payouts_locks
            .entry(user_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

pub async fn process_payout(
    pool: &PgPool,
    redis: &RedisPool,
    client: &clickhouse::Client,
) -> Result<(), ApiError> {
    let start: DateTime<Utc> = DateTime::from_naive_utc_and_offset(
        (Utc::now() - Duration::days(1))
            .date_naive()
            .and_hms_nano_opt(0, 0, 0, 0)
            .unwrap_or_default(),
        Utc,
    );

    let results = sqlx::query!(
        "SELECT EXISTS(SELECT 1 FROM payouts_values WHERE created = $1)",
        start,
    )
    .fetch_one(pool)
    .await?;

    if results.exists.unwrap_or(false) {
        return Ok(());
    }

    let end = start + Duration::days(1);
    #[derive(Deserialize, clickhouse::Row)]
    struct ProjectMultiplier {
        pub page_views: u64,
        pub project_id: u64,
    }

    let (views_values, views_sum, downloads_values, downloads_sum) = futures::future::try_join4(
        client
            .query(
                r#"
                SELECT COUNT(1) page_views, project_id
                FROM views
                WHERE (recorded BETWEEN ? AND ?) AND (project_id != 0)
                GROUP BY project_id
                ORDER BY page_views DESC
                "#,
            )
            .bind(start.timestamp())
            .bind(end.timestamp())
            .fetch_all::<ProjectMultiplier>(),
        client
            .query("SELECT COUNT(1) FROM views WHERE (recorded BETWEEN ? AND ?) AND (project_id != 0)")
            .bind(start.timestamp())
            .bind(end.timestamp())
            .fetch_one::<u64>(),
        client
            .query(
                r#"
                SELECT COUNT(1) page_views, project_id
                FROM downloads
                WHERE (recorded BETWEEN ? AND ?) AND (user_id != 0)
                GROUP BY project_id
                ORDER BY page_views DESC
                "#,
            )
            .bind(start.timestamp())
            .bind(end.timestamp())
            .fetch_all::<ProjectMultiplier>(),
        client
            .query("SELECT COUNT(1) FROM downloads WHERE (recorded BETWEEN ? AND ?) AND (user_id != 0)")
            .bind(start.timestamp())
            .bind(end.timestamp())
            .fetch_one::<u64>(),
    )
        .await?;

    let mut transaction = pool.begin().await?;

    struct PayoutMultipliers {
        sum: u64,
        values: HashMap<u64, u64>,
    }

    let mut views_values = views_values
        .into_iter()
        .map(|x| (x.project_id, x.page_views))
        .collect::<HashMap<u64, u64>>();
    let downloads_values = downloads_values
        .into_iter()
        .map(|x| (x.project_id, x.page_views))
        .collect::<HashMap<u64, u64>>();

    for (key, value) in downloads_values.iter() {
        let counter = views_values.entry(*key).or_insert(0);
        *counter += *value;
    }

    let multipliers: PayoutMultipliers = PayoutMultipliers {
        sum: downloads_sum + views_sum,
        values: views_values,
    };

    struct Project {
        // user_id, payouts_split
        team_members: Vec<(i64, Decimal)>,
    }

    let mut projects_map: HashMap<i64, Project> = HashMap::new();

    use futures::TryStreamExt;

    sqlx::query!(
        "
        SELECT m.id id, tm.user_id user_id, tm.payouts_split payouts_split
        FROM mods m
        INNER JOIN team_members tm on m.team_id = tm.team_id AND tm.accepted = TRUE
        WHERE m.id = ANY($1) AND m.monetization_status = $2
        ",
        &multipliers
            .values
            .keys()
            .map(|x| *x as i64)
            .collect::<Vec<i64>>(),
        MonetizationStatus::Monetized.as_str(),
    )
    .fetch_many(&mut *transaction)
    .try_for_each(|e| {
        if let Some(row) = e.right() {
            if let Some(project) = projects_map.get_mut(&row.id) {
                project.team_members.push((row.user_id, row.payouts_split));
            } else {
                projects_map.insert(
                    row.id,
                    Project {
                        team_members: vec![(row.user_id, row.payouts_split)],
                    },
                );
            }
        }

        futures::future::ready(Ok(()))
    })
    .await?;

    let amount = Decimal::from(parse_var::<u64>("PAYOUTS_BUDGET").unwrap_or(0));

    let days = Decimal::from(28);
    let weekdays = Decimal::from(20);
    let weekend_bonus = Decimal::from(5) / Decimal::from(4);

    let weekday_amount = amount / (weekdays + (weekend_bonus) * (days - weekdays));
    let weekend_amount = weekday_amount * weekend_bonus;

    let payout = match start.weekday() {
        Weekday::Sat | Weekday::Sun => weekend_amount,
        _ => weekday_amount,
    };

    let mut clear_cache_users = Vec::new();
    let (mut insert_user_ids, mut insert_project_ids, mut insert_payouts, mut insert_starts) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (id, project) in projects_map {
        if let Some(value) = &multipliers.values.get(&(id as u64)) {
            let project_multiplier: Decimal =
                Decimal::from(**value) / Decimal::from(multipliers.sum);

            let sum_splits: Decimal = project.team_members.iter().map(|x| x.1).sum();

            if sum_splits > Decimal::ZERO {
                for (user_id, split) in project.team_members {
                    let payout: Decimal = payout * project_multiplier * (split / sum_splits);

                    if payout > Decimal::ZERO {
                        insert_user_ids.push(user_id);
                        insert_project_ids.push(id);
                        insert_payouts.push(payout);
                        insert_starts.push(start);

                        sqlx::query!(
                            "
                            UPDATE users
                            SET balance = balance + $1
                            WHERE id = $2
                            ",
                            payout,
                            user_id
                        )
                        .execute(&mut *transaction)
                        .await?;

                        clear_cache_users.push(user_id);
                    }
                }
            }
        }
    }

    sqlx::query!(
        "
        INSERT INTO payouts_values (user_id, mod_id, amount, created)
        SELECT * FROM UNNEST ($1::bigint[], $2::bigint[], $3::numeric[], $4::timestamptz[])
        ",
        &insert_user_ids[..],
        &insert_project_ids[..],
        &insert_payouts[..],
        &insert_starts[..]
    )
    .execute(&mut *transaction)
    .await?;

    transaction.commit().await?;

    if !clear_cache_users.is_empty() {
        crate::database::models::User::clear_caches(
            &clear_cache_users
                .into_iter()
                .map(|x| (crate::database::models::UserId(x), None))
                .collect::<Vec<_>>(),
            redis,
        )
        .await?;
    }

    Ok(())
}

// Used for testing, should be the same as the above function
pub async fn insert_payouts(
    insert_user_ids: Vec<i64>,
    insert_project_ids: Vec<i64>,
    insert_payouts: Vec<Decimal>,
    insert_starts: Vec<DateTime<Utc>>,
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> sqlx::Result<PgQueryResult> {
    sqlx::query!(
        "
        INSERT INTO payouts_values (user_id, mod_id, amount, created)
        SELECT * FROM UNNEST ($1::bigint[], $2::bigint[], $3::numeric[], $4::timestamptz[])
        ",
        &insert_user_ids[..],
        &insert_project_ids[..],
        &insert_payouts[..],
        &insert_starts[..]
    )
    .execute(&mut **transaction)
    .await
}
