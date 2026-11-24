//! An in-memory `SlackApi` for tests. Each method answers from its own queue of scripted answers,
//! in order; a method with nothing queued fails with `ApiError::Other { code: "unscripted" }`.
//! Every call is logged with its method and arguments, never a token.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use futures::FutureExt as _;
use futures::future::BoxFuture;

use crate::api::{ApiError, AuthInfo, PostRequest, PostedMessage, SlackApi, User};
use crate::types::{Conversation, ConversationId, Cursor, Message, Page, Ts, UserId};

/// One call made to the fake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    AuthTest,
    Conversations {
        cursor: Option<Cursor>,
    },
    History {
        channel: ConversationId,
        latest: Option<Ts>,
        limit: usize,
    },
    Replies {
        channel: ConversationId,
        thread_ts: Ts,
        cursor: Option<Cursor>,
    },
    User {
        id: UserId,
    },
    Users {
        cursor: Option<Cursor>,
    },
    Post(PostRequest),
}

type Answers<T> = VecDeque<Result<T, ApiError>>;

#[derive(Default)]
struct Inner {
    calls: Vec<Call>,
    auth_test: Answers<AuthInfo>,
    conversations: Answers<Page<Conversation>>,
    history: Answers<Page<Message>>,
    replies: Answers<Page<Message>>,
    user: Answers<User>,
    users: Answers<Page<User>>,
    post: Answers<PostedMessage>,
}

#[derive(Clone, Default)]
pub struct FakeSlack {
    inner: Arc<Mutex<Inner>>,
}

fn unscripted() -> ApiError {
    ApiError::Other {
        code: "unscripted".to_string(),
    }
}

impl FakeSlack {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Every call so far, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    pub fn push_auth_test(&self, answer: Result<AuthInfo, ApiError>) {
        self.lock().auth_test.push_back(answer);
    }

    pub fn push_conversations(&self, answer: Result<Page<Conversation>, ApiError>) {
        self.lock().conversations.push_back(answer);
    }

    pub fn push_history(&self, answer: Result<Page<Message>, ApiError>) {
        self.lock().history.push_back(answer);
    }

    pub fn push_replies(&self, answer: Result<Page<Message>, ApiError>) {
        self.lock().replies.push_back(answer);
    }

    pub fn push_user(&self, answer: Result<User, ApiError>) {
        self.lock().user.push_back(answer);
    }

    pub fn push_users(&self, answer: Result<Page<User>, ApiError>) {
        self.lock().users.push_back(answer);
    }

    pub fn push_post(&self, answer: Result<PostedMessage, ApiError>) {
        self.lock().post.push_back(answer);
    }

    fn answer<T: Send + 'static>(
        &self,
        call: Call,
        queue: impl FnOnce(&mut Inner) -> &mut Answers<T>,
    ) -> BoxFuture<'static, Result<T, ApiError>> {
        let mut inner = self.lock();
        inner.calls.push(call);
        let answer = queue(&mut inner)
            .pop_front()
            .unwrap_or_else(|| Err(unscripted()));
        futures::future::ready(answer).boxed()
    }
}

impl SlackApi for FakeSlack {
    fn auth_test(&self) -> BoxFuture<'static, Result<AuthInfo, ApiError>> {
        self.answer(Call::AuthTest, |inner| &mut inner.auth_test)
    }

    fn conversations(
        &self,
        cursor: Option<Cursor>,
    ) -> BoxFuture<'static, Result<Page<Conversation>, ApiError>> {
        self.answer(Call::Conversations { cursor }, |inner| {
            &mut inner.conversations
        })
    }

    fn history(
        &self,
        channel: ConversationId,
        latest: Option<Ts>,
        limit: usize,
    ) -> BoxFuture<'static, Result<Page<Message>, ApiError>> {
        self.answer(
            Call::History {
                channel,
                latest,
                limit,
            },
            |inner| &mut inner.history,
        )
    }

    fn replies(
        &self,
        channel: ConversationId,
        thread_ts: Ts,
        cursor: Option<Cursor>,
    ) -> BoxFuture<'static, Result<Page<Message>, ApiError>> {
        self.answer(
            Call::Replies {
                channel,
                thread_ts,
                cursor,
            },
            |inner| &mut inner.replies,
        )
    }

    fn user(&self, id: UserId) -> BoxFuture<'static, Result<User, ApiError>> {
        self.answer(Call::User { id }, |inner| &mut inner.user)
    }

    fn users(&self, cursor: Option<Cursor>) -> BoxFuture<'static, Result<Page<User>, ApiError>> {
        self.answer(Call::Users { cursor }, |inner| &mut inner.users)
    }

    fn post(&self, request: PostRequest) -> BoxFuture<'static, Result<PostedMessage, ApiError>> {
        self.answer(Call::Post(request), |inner| &mut inner.post)
    }
}
