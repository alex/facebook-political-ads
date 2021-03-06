use chrono::DateTime;
use chrono::offset::Utc;
use diesel;
use diesel::prelude::*;
use diesel::pg::PgConnection;
use diesel::pg::upsert::*;
use diesel::types::Text;
use diesel_full_text_search::*;
use errors::*;
use futures::{Future, stream, Stream};
use futures_cpupool::CpuPool;
use hyper::{Body, Client, Uri};
use hyper::client::HttpConnector;
use hyper_tls::HttpsConnector;
use kuchiki;
use kuchiki::iter::{Select, Elements, Descendants};
use kuchiki::traits::*;
use url::Url;
use r2d2_diesel::ConnectionManager;
use r2d2::Pool;
use rusoto_core::{default_tls_client, Region};
use rusoto_credential::DefaultCredentialsProvider;
use rusoto_s3::{PutObjectRequest, S3Client, S3};
use schema::ads;
use std::collections::HashMap;
use server::AdPost;

const ENDPOINT: &'static str = "https://pp-facebook-ads.s3.amazonaws.com/";

fn document_select(
    document: &kuchiki::NodeRef,
    selector: &str,
) -> Result<Select<Elements<Descendants>>> {
    document.select(selector).map_err(|_| {
        ErrorKind::HTML(format!("Selector compile error {}", selector)).into()
    })
}

pub fn get_title(document: &kuchiki::NodeRef) -> Result<String> {
    document_select(document, "h5 a, h6 a, strong, span.fsl")?
        .nth(0)
        .and_then(|a| Some(a.text_contents()))
        .ok_or_else(|| "Couldn't find title.".into())
}

fn get_image(document: &kuchiki::NodeRef) -> Result<String> {
    document_select(document, "img")
        .map_err(|_| ErrorKind::HTML("Selector compile error".to_string()))?
        .nth(0)
        .and_then(|a| {
            a.attributes.borrow().get("src").and_then(
                |src| Some(src.to_string()),
            )
        })
        .ok_or_else(|| "Couldn't find images.".into())
}

pub fn get_message(document: &kuchiki::NodeRef) -> Result<String> {
    let selectors = vec![".userContent p", "div.mbs", "span"];
    let iters = selectors
        .iter()
        .map(|s| document_select(document, s))
        .flat_map(|a| a);

    iters
        .map(|i| {
            i.fold(String::new(), |m, a| m + &a.as_node().to_string())
        })
        .filter(|i| !i.is_empty())
        .nth(0)
        .ok_or_else(|| "Couldn't find message.".into())
}

fn get_images(document: &kuchiki::NodeRef) -> Result<Vec<String>> {
    let select = document_select(document, "img")?;
    Ok(
        select
            .skip(1)
            .map(|a| {
                a.attributes.borrow().get("src").and_then(
                    |s| Some(s.to_string()),
                )
            })
            .filter(|s| s.is_some())
            .map(|s| s.unwrap())
            .collect::<Vec<String>>(),
    )
}


fn get_real_image_uri(uri: Uri) -> Uri {
    let url = uri.to_string().parse::<Url>();
    let query_map: HashMap<_, _> = url.unwrap().query_pairs().into_owned().collect();
    query_map.get("url")
        .map(|u| u.parse::<Uri>()) // Option<Result>
        .unwrap_or_else(|| Ok(uri.clone())) // Result
        .unwrap_or(uri) // Uri
}

#[derive(AsChangeset, Debug)]
#[table_name = "ads"]
pub struct Images {
    thumbnail: Option<String>,
    images: Vec<String>,
    title: String,
    message: String,
    html: String,
}

impl Images {
    fn from_ad(ad: &Ad, images: Vec<Uri>) -> Result<Images> {
        let thumb = images
            .iter()
            .filter(|i| ad.thumbnail.contains(i.path()))
            .map(|i| ENDPOINT.to_string() + i.path().trim_left_matches('/'))
            .nth(0);

        let mut rest = images.clone();
        if let Some(thumb) = thumb.clone() {
            rest.retain(|x| !thumb.contains(x.path()))
        };

        let collection = rest.iter()
            .filter(|i| ad.images.iter().any(|a| a.contains(i.path())))
            .map(|i| ENDPOINT.to_string() + i.path().trim_left_matches('/'))
            .collect::<Vec<String>>();

        let document = kuchiki::parse_html().one(ad.html.clone());
        for a in document_select(&document, "img")? {
            if let Some(x) = a.attributes.borrow_mut().get_mut("src") {
                if let Ok(u) = x.parse::<Uri>() {
                    if let Some(i) = images.iter().find(|i| {
                        i.path() == get_real_image_uri(u.clone()).path()
                    })
                    {
                        *x = ENDPOINT.to_string() + i.path().trim_left_matches('/');
                    } else {
                        *x = "".to_string();
                    }
                }
            };
        }

        let title = get_title(&document)?;
        let message = get_message(&document)?;
        Ok(Images {
            thumbnail: thumb,
            images: collection,
            title: title,
            html: document_select(&document, "div")?
                .nth(0)
                .ok_or("Couldn't find a div in the html")?
                .as_node()
                .to_string(),
            message: message,
        })
    }
}

#[derive(Serialize, Queryable, Debug, Clone)]
pub struct Ad {
    pub id: String,
    pub html: String,
    pub political: i32,
    pub not_political: i32,
    pub title: String,
    pub message: String,
    pub thumbnail: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub lang: String,
    pub images: Vec<String>,
    pub impressions: i32,
    pub political_probability: f64,
    pub targeting: Option<String>,
    #[serde(skip_serializing)]
    pub suppressed: bool,
}

// We do this because I can't see how to make sql_function! take a string
// argument.
sql_function!(to_englishtsvector, to_englishtsvector_t, (x: Text) -> TsVector);
sql_function!(to_germantsvector, to_germantsvector_t, (x: Text) -> TsVector);
sql_function!(to_englishtsquery, to_englishtsquery_t, (x: Text) -> TsQuery);
sql_function!(to_germantsquery, to_germantsquery_t, (x: Text) -> TsQuery);

impl Ad {
    // This will asynchronously save the images to s3 we may very well end up
    // dropping images, but I can't see any way around it right now. Also we
    // should think about splitting this up, but I'm fine -- if a little
    // embarassed about it -- right now. This function swallows errors, and
    // there's a chance we'll end up with no images at the end, but I think we
    // can handle that in the extension's UI.
    pub fn grab_and_store(
        &self,
        client: Client<HttpsConnector<HttpConnector>, Body>,
        db: &Pool<ConnectionManager<PgConnection>>,
        pool: CpuPool,
    ) -> Box<Future<Item = (), Error = ()>> {
        let ad = self.clone();
        let pool_s3 = pool.clone();
        let pool_db = pool.clone();
        let db = db.clone();
        let future = stream::iter_ok(self.image_urls())
            // filter ones we already have in the db and ones we can verify as
            // coming from fb, we don't want to become a malware vector :)
            // currently we redownload images we already have, but ok.
            .filter(|u| {
                info!("testing {:?}", u.host());
                match u.host() {
                    Some(h) => (h == "pp-facebook-ads.s3.amazonaws.com" || h.ends_with("fbcdn.net")),
                    None => false
                }
            })
            // grab image
            .and_then(move |img| {
                let real_url = get_real_image_uri(img);
                info!("getting {:?}", real_url.path());
                client
                    .get(real_url.clone())
                    .and_then(|res| {
                        res.body().concat2().and_then(|chunk| Ok((chunk, real_url)))
                    })
                    .map_err(|e| Error::with_chain(e, "Could not get image"))
            })
            // upload them to s3
            .and_then(move |tuple| {
                let pool = pool_s3.clone();
                // we do this in a worker thread because rusoto isn't on
                // Hyper async yet.
                pool.spawn_fn(move || {
                    if tuple.1.host().unwrap() != "pp-facebook-ads.s3.amazonaws.com" {
                        let credentials = DefaultCredentialsProvider::new()?;
                        let tls = default_tls_client()?;
                        let client = S3Client::new(tls, credentials, Region::UsEast1);
                        let req = PutObjectRequest {
                            bucket: "pp-facebook-ads".to_string(),
                            key: tuple.1.path().trim_left_matches('/').to_string(),
                            acl: Some("public-read".to_string()),
                            body: Some(tuple.0.to_vec()),
                            ..PutObjectRequest::default()
                        };
                        client.put_object(&req)?;
                    }
                    Ok(tuple.1)
                })
            })
            .collect()
            // save the new urls to the database. the images variable will
            // include only those that we've successfully saved to s3, so we
            // have to do a funky merge here.
            .and_then(move |images| {
                let imgs = images.clone();
                pool_db.spawn_fn(move || {
                    use schema::ads::dsl::*;
                    let update = Images::from_ad(&ad, imgs)?;
                    let connection = db.get()?;
                    diesel::update(ads.find(&ad.id))
                        .set(&update)
                        .execute(&*connection)?;
                    info!("saved {:?}", ad.id);
                    Ok(())
                })
            })
            .map_err(|e| {
                warn!("{:?}", e);
                ()
            });
        Box::new(future)
    }

    pub(self) fn image_urls(&self) -> Vec<Uri> {
        let images = [vec![self.thumbnail.clone()], self.images.clone()];
        images
            .concat()
            .iter()
            .flat_map(|a| a.parse::<Uri>())
            .collect()
    }

    pub fn get_ads_by_lang(
        language: &str,
        conn: &Pool<ConnectionManager<PgConnection>>,
        options: &HashMap<String, String>,
    ) -> Result<Vec<Ad>> {
        use schema::ads::dsl::*;
        let connection = conn.get()?;
        let mut query = ads.filter(lang.eq(language))
            .filter(political_probability.gt(0.80))
            .filter(suppressed.eq(false))
            .into_boxed();

        if let Some(search) = options.get("search") {
            query = match language {
                "de-DE" => {
                    query
                        .filter(to_germantsvector(html).matches(
                            to_germantsquery(search.clone()),
                        ))
                        .order(ts_rank(to_germantsvector(html), to_germantsquery(search)))
                }
                _ => {
                    query
                        .filter(to_englishtsvector(html).matches(
                            to_englishtsquery(search.clone()),
                        ))
                        .order(ts_rank(to_englishtsvector(html), to_englishtsquery(search)))
                }
            }
        }

        if let Some(page) = options.get("page") {
            let raw_offset = page.parse::<usize>().unwrap_or_default() * 20;
            let offset = if raw_offset > 1000 { 1000 } else { raw_offset };
            query = query.offset(offset as i64)
        }

        Ok(query.order(created_at.desc()).limit(20).load::<Ad>(
            &*connection,
        )?)
    }

    pub fn suppress(adid: String, conn: &Pool<ConnectionManager<PgConnection>>) -> Result<()> {
        use schema::ads::dsl::*;
        let connection = conn.get()?;
        {
            warn!("Suppressed {:?}", adid);
        }
        diesel::update(ads.filter(id.eq(adid)))
            .set(suppressed.eq(true))
            .execute(&*connection)?;
        Ok(())
    }
}

#[derive(Insertable)]
#[table_name = "ads"]
pub struct NewAd<'a> {
    id: &'a str,
    html: &'a str,
    political: i32,
    not_political: i32,

    title: String,
    message: String,
    thumbnail: String,

    lang: &'a str,
    images: Vec<String>,
    impressions: i32,

    targeting: Option<String>,
}


impl<'a> NewAd<'a> {
    pub fn new(ad: &'a AdPost, lang: &'a str) -> Result<NewAd<'a>> {
        info!("saving {}", ad.id);
        let document = kuchiki::parse_html().one(ad.html.clone());

        let thumb = get_image(&document)?;
        let images = get_images(&document)?;
        let message = get_message(&document)?;
        let title = get_title(&document)?;

        Ok(NewAd {
            id: &ad.id,
            html: &ad.html,
            // we try unwrapping or we chose the false branch in both of these
            // cases to count impressions
            political: if ad.political.unwrap_or(false) { 1 } else { 0 },
            not_political: if !ad.political.unwrap_or(true) { 1 } else { 0 },
            title: title,
            message: message,
            thumbnail: thumb,
            lang: lang,
            images: images,
            impressions: if !ad.political.is_some() { 1 } else { 0 },
            targeting: ad.targeting.clone(),
        })
    }

    pub fn save(&self, pool: &Pool<ConnectionManager<PgConnection>>) -> Result<Ad> {
        use schema::ads;
        use schema::ads::dsl::*;
        let connection = pool.get()?;

        // increment impressions if this is a background save,
        // otherwise increment political counters
        let ad: Ad = diesel::insert(&self.on_conflict(
            id,
            do_update().set((
                political.eq(political + self.political),
                not_political.eq(
                    not_political +
                        self.not_political,
                ),
                impressions.eq(
                    impressions + self.impressions,
                ),
                updated_at.eq(Utc::now()),
            )),
        )).into(ads::table)
            .get_result(&*connection)?;

        if self.targeting.is_some() && !ad.targeting.is_some() {
            diesel::update(ads.find(self.id))
                .set(targeting.eq(&self.targeting))
                .execute(&*connection)?;
        };

        Ok(ad)
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ad_parsing() {
        let ad = include_str!("./html-test.txt");
        let post = AdPost {
            id: "test".to_string(),
            html: ad.to_string(),
            political: None,
            targeting: None,
        };
        let new_ad = NewAd::new(&post, "en-US").unwrap();
        assert!(new_ad.thumbnail.len() > 0);
        assert_eq!(new_ad.images.len(), 2);
        assert!(new_ad.title.len() > 0);

        assert_eq!(
            kuchiki::parse_html().one(new_ad.message).text_contents(),
            kuchiki::parse_html().one("<p><a class=\"_58cn\" href=\"https://www.facebook.com/hashtag/valerian\"><span class=\"_5afx\"><span class=\"_58cl _5afz\">#</span><span class=\"_58cm\">Valerian</span></span></a> is “the best experience since ‘Avatar.’” See it in 3D and RealD3D theaters this Friday. Get tickets now: <a>ValerianTickets.com</a></p>").text_contents()
        );
    }

    #[test]
    fn image_parsing() {
        use chrono::prelude::*;
        let ad = include_str!("./html-test.txt");
        let document = kuchiki::parse_html().one(ad);

        let saved_ad = Ad {
            id: "test".to_string(),
            html: ad.to_string(),
            political: 1,
            not_political: 2,
            title: get_title(&document).unwrap(),
            message: get_message(&document).unwrap(),
            thumbnail: get_image(&document).unwrap(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            lang: "US".to_string(),
            images: get_images(&document).unwrap(),
            impressions: 1,
            targeting: None,
            political_probability: 0.0,
            suppressed: false,
        };
        let urls = saved_ad
            .image_urls()
            .into_iter()
            .map(|x| x.unwrap())
            .collect();
        let images = Images::from_ad(&saved_ad, urls).unwrap();
        assert!(images.html != saved_ad.html);
        assert!(!images.html.contains("fbcdn"));
        assert!(!images.html.contains("html"));
        assert!(images.images.len() == saved_ad.images.len());
        assert!(images.thumbnail.unwrap() != saved_ad.thumbnail);
    }
}
