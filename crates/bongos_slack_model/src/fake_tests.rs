use futures::executor::block_on;

use crate::api::{ApiError, PostRequest, PostedMessage, SlackApi as _};
use crate::fake::{Call, FakeSlack};
use crate::types::{ConversationId, Cursor, Page, Ts, UserId};

#[test]
fn fake_slack_answers_in_order_and_logs_every_call() {
    let fake = FakeSlack::new();
    let channel = ConversationId("C0123".to_string());
    let posted = PostedMessage {
        channel: channel.clone(),
        ts: Ts::new("1730000000.000200"),
    };
    fake.push_post(Ok(posted.clone()));
    fake.push_post(Err(ApiError::NotInChannel));
    fake.push_history(Ok(Page {
        items: Vec::new(),
        next: None,
    }));

    let request = PostRequest {
        channel: channel.clone(),
        text: "hello".to_string(),
        thread_ts: Some(Ts::new("1730000000.000100")),
    };
    assert_eq!(block_on(fake.post(request.clone())), Ok(posted));
    assert_eq!(
        block_on(fake.post(request.clone())),
        Err(ApiError::NotInChannel)
    );
    assert!(block_on(fake.history(channel.clone(), None, 50)).is_ok());
    assert_eq!(
        block_on(fake.user(UserId("U1".to_string()))),
        Err(ApiError::Other {
            code: "unscripted".to_string()
        })
    );
    assert!(block_on(fake.conversations(Some(Cursor("next".to_string())))).is_err());

    assert_eq!(
        fake.calls(),
        vec![
            Call::Post(request.clone()),
            Call::Post(request),
            Call::History {
                channel,
                latest: None,
                limit: 50
            },
            Call::User {
                id: UserId("U1".to_string())
            },
            Call::Conversations {
                cursor: Some(Cursor("next".to_string()))
            },
        ]
    );
}
