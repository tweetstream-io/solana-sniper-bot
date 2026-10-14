//! The bot's local view of each post and each account profile. `tweet/update` patches the stored
//! post, `tweet/meta` replaces its snapshot. Next: `detect/post.rs` reads the merged post.

use std::collections::{HashMap, VecDeque};

use crate::protocol::{Author, Field, PostKind, ProfileFields, Reference, TweetMeta, TweetPost, TweetUrl};

/// A post as merged from its content and updates.
#[derive(Debug, Clone, Default)]
pub struct Post {
    /// Post id.
    pub tweet_id: String,
    /// Text, once known.
    pub text: Option<String>,
    /// What the post is.
    pub kind: Option<PostKind>,
    /// The author's fields, merged field by field.
    pub author: Author,
    /// The post it references.
    pub reference: Option<Reference>,
    /// Attached links.
    pub urls: Option<Vec<TweetUrl>>,
    /// Publish time from the payload.
    pub created_at: Option<f64>,
}

impl Post {
    fn patch(&mut self, patch: TweetPost) {
        let TweetPost { tweet_id, platform: _, text, created_at, kind, author, reference, urls } = patch;
        self.tweet_id = tweet_id;
        if text.is_some() {
            self.text = text;
        }
        if created_at.is_some() {
            self.created_at = created_at;
        }
        if kind.is_some() {
            self.kind = kind;
        }
        if let Some(author) = author {
            merge_author(&mut self.author, author);
        }
        if reference.is_some() {
            self.reference = reference;
        }
        if urls.is_some() {
            self.urls = urls;
        }
    }

    fn into_patch(self) -> TweetPost {
        let Post { tweet_id, text, kind, author, reference, urls, created_at } = self;
        TweetPost { tweet_id, platform: None, text, created_at, kind, author: Some(author), reference, urls }
    }
}

fn merge_author(base: &mut Author, patch: Author) {
    let Author { id, handle, name, bio, platform, website_url } = patch;
    let slots = [
        (&mut base.id, id),
        (&mut base.handle, handle),
        (&mut base.name, name),
        (&mut base.bio, bio),
        (&mut base.platform, platform),
        (&mut base.website_url, website_url),
    ];
    for (slot, value) in slots {
        if value.is_some() {
            *slot = value;
        }
    }
}

/// A stored post and its latest `tweet/meta` snapshot.
#[derive(Debug, Default)]
pub struct Entry {
    /// The merged post, once any content or update arrived.
    pub post: Option<Post>,
    /// The latest meta snapshot.
    pub meta: Option<TweetMeta>,
}

/// The last known value of each profile field, the baseline for profile diffs.
#[derive(Debug, Clone, Default)]
pub struct KnownProfile {
    /// Avatar URL.
    pub avatar: Field,
    /// Banner URL.
    pub banner: Field,
    /// Bio.
    pub bio: Field,
    /// Display name.
    pub name: Field,
    /// Website.
    pub website_url: Field,
}

const MAX_POSTS: usize = 20_000;
const MAX_PROFILES: usize = 20_000;

/// Posts by post key and profiles by `<platform>:<account id>`, each bounded. Posts are evicted in
/// arrival order, profiles least recently updated first.
#[derive(Debug, Default)]
pub struct PostStore {
    posts: HashMap<String, Entry>,
    post_order: VecDeque<String>,
    profiles: HashMap<String, (KnownProfile, u64)>,
    profile_order: VecDeque<(String, u64)>,
    generation: u64,
}

impl PostStore {
    /// Returns the stored entry for a post key.
    pub fn get(&self, key: &str) -> Option<&Entry> {
        self.posts.get(key)
    }

    /// Applies `tweet/content`. An update that arrived before its content still wins for the
    /// fields it set. The author's bio and name seed the profile baseline.
    pub fn apply_content(&mut self, key: &str, platform: &str, content: TweetPost) {
        if let Some(author) = &content.author
            && let Some(id) = &author.id
        {
            let to_field = |value: &Option<String>| value.clone().map_or(Field::Absent, Field::Value);
            let seen = KnownProfile { bio: to_field(&author.bio), name: to_field(&author.name), ..KnownProfile::default() };
            self.merge_profile(&profile_key(platform, id), seen);
        }
        let entry = self.entry(key);
        let mut post = Post::default();
        post.patch(content);
        if let Some(earlier) = entry.post.take() {
            post.patch(earlier.into_patch());
        }
        entry.post = Some(post);
    }

    /// Applies `tweet/update`: patches only the fields present. A `ref` in the update replaces the
    /// stored one.
    pub fn apply_update(&mut self, key: &str, update: TweetPost) {
        self.entry(key).post.get_or_insert_with(Post::default).patch(update);
    }

    /// Applies `tweet/meta`, a complete snapshot: replace, never merge.
    pub fn apply_meta(&mut self, key: &str, meta: TweetMeta) {
        self.entry(key).meta = Some(meta);
    }

    /// Returns the last known profile of an account.
    pub fn profile(&self, platform: &str, account_id: Option<&str>) -> Option<&KnownProfile> {
        self.profiles.get(&profile_key(platform, account_id?)).map(|(profile, _)| profile)
    }

    /// Merges the fields present in a profile change into the account's known profile.
    pub fn update_profile(&mut self, platform: &str, account_id: Option<&str>, fields: &ProfileFields) {
        let Some(id) = account_id else {
            return;
        };
        let ProfileFields { avatar, banner, bio, handle: _, name, website_url } = fields.clone();
        self.merge_profile(&profile_key(platform, id), KnownProfile { avatar, banner, bio, name, website_url });
    }

    fn merge_profile(&mut self, key: &str, seen: KnownProfile) {
        self.generation += 1;
        let generation = self.generation;
        let (profile, stamp) = self.profiles.entry(key.to_string()).or_default();
        let KnownProfile { avatar, banner, bio, name, website_url } = seen;
        let slots = [
            (&mut profile.avatar, avatar),
            (&mut profile.banner, banner),
            (&mut profile.bio, bio),
            (&mut profile.name, name),
            (&mut profile.website_url, website_url),
        ];
        for (slot, value) in slots {
            if value.is_present() {
                *slot = value;
            }
        }
        *stamp = generation;
        self.profile_order.push_back((key.to_string(), generation));
        self.evict_profiles();
    }

    /// `profile_order` keeps one stamp per update, so an entry whose stamp is stale was updated
    /// again later and is skipped.
    fn evict_profiles(&mut self) {
        while self.profiles.len() > MAX_PROFILES {
            let Some((key, generation)) = self.profile_order.pop_front() else {
                return;
            };
            if self.profiles.get(&key).is_some_and(|(_, stamp)| *stamp == generation) {
                self.profiles.remove(&key);
            }
        }
        if self.profile_order.len() > 4 * MAX_PROFILES {
            let profiles = &self.profiles;
            self.profile_order.retain(|(key, generation)| profiles.get(key).is_some_and(|(_, stamp)| stamp == generation));
        }
    }

    fn entry(&mut self, key: &str) -> &mut Entry {
        if !self.posts.contains_key(key) {
            self.posts.insert(key.to_string(), Entry::default());
            self.post_order.push_back(key.to_string());
            if self.posts.len() > MAX_POSTS
                && let Some(oldest) = self.post_order.pop_front()
            {
                self.posts.remove(&oldest);
            }
        }
        self.posts.get_mut(key).expect("inserted above")
    }
}

fn profile_key(platform: &str, id: &str) -> String {
    format!("{platform}:{id}")
}
