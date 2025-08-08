use std::pin::Pin;
use std::task::{Context, Poll};

use futures::future::BoxFuture;
use futures::prelude::*;
use kube::{Api, api};

use super::filter_params::FilterParams;

const PAGE_SIZE: u32 = 500;

type ListResponse<T> = BoxFuture<'static, (kube::Result<api::ObjectList<T>>, Api<T>)>;

fn list_request<T>(api: Api<T>, list_params: &api::ListParams) -> ListResponse<T>
where
    T: Clone + serde::de::DeserializeOwned + std::fmt::Debug + 'static,
{
    let list_params = list_params.clone();
    async move { (api.list(&list_params).await, api) }.boxed()
}

pub struct ListStream<T>
where
    T: Clone,
{
    list_params: api::ListParams,
    remaining_item_count: Option<i64>,
    resource_version: Option<String>,
    items: Vec<T>,
    next: Option<ListResponse<T>>,
}

pub struct ListStreamItem<T> {
    pub resource_version: Option<String>,
    pub remaining_item_count: Option<i64>,
    pub item: T,
}

impl<T> ListStream<T>
where
    T: Clone + serde::de::DeserializeOwned + std::fmt::Debug + 'static,
{
    pub fn new(api: Api<T>, list_params: &FilterParams) -> Self {
        let mut list_params: api::ListParams = list_params.into();
        list_params.limit = Some(PAGE_SIZE);
        Self {
            next: Some(list_request(api, &list_params)),
            list_params,
            remaining_item_count: None,
            resource_version: None,
            items: vec![],
        }
    }

    fn pop(&mut self) -> Option<ListStreamItem<T>> {
        if let Some(item) = self.items.pop() {
            if let Some(count) = self.remaining_item_count.as_mut() {
                *count -= 1
            }
            Some(ListStreamItem {
                resource_version: self.resource_version.clone(),
                remaining_item_count: self.remaining_item_count,
                item,
            })
        } else {
            None
        }
    }
}

impl<T> Stream for ListStream<T>
where
    T: Clone + serde::de::DeserializeOwned + std::fmt::Debug + Unpin + 'static,
{
    type Item = kube::Result<ListStreamItem<T>>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(item) = this.pop() {
                return Poll::Ready(Some(Ok(item)));
            }
            if let Some(mut next) = this.next.take() {
                match next.as_mut().poll(cx) {
                    Poll::Ready((Ok(list), api)) => {
                        this.resource_version = list.metadata.resource_version.clone();
                        this.remaining_item_count = list
                            .metadata
                            .remaining_item_count
                            .map(|count| count + this.items.len() as i64);
                        this.items = list.items;
                        if let Some(token) = list.metadata.continue_ {
                            if !token.is_empty() {
                                this.list_params.continue_token = Some(token);
                                this.next = Some(list_request(api, &this.list_params));
                            }
                        }
                    }
                    Poll::Ready((Err(err), _)) => {
                        return Poll::Ready(Some(Err(err)));
                    }
                    Poll::Pending => {
                        this.next = Some(next);
                        return Poll::Pending;
                    }
                }
            } else {
                return Poll::Ready(None);
            }
        }
    }
}

pub trait ApiListStreamExt<T> {
    fn list_stream(&self, list_params: &FilterParams) -> ListStream<T>
    where
        T: Clone;
}

impl<T> ApiListStreamExt<T> for Api<T>
where
    T: Clone + serde::de::DeserializeOwned + std::fmt::Debug + 'static,
{
    fn list_stream(&self, list_params: &FilterParams) -> ListStream<T> {
        ListStream::new(self.clone(), list_params)
    }
}
