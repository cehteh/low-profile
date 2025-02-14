use core::{marker::PhantomData, mem::MaybeUninit};

use crate::{
    error::ProtocolError,
    handler,
    parse::PathAndQuery,
    request::{record_header_indices, Body, HeaderIndices, Headers, Parts},
    route::{self, Route},
    service::ServiceError,
    utils, ErrorType, IntoResponse, Method, Read, Request, Service, Write,
};

mod private {
    #[derive(Debug, Clone, Copy)]
    pub enum HasAnyState {}

    #[derive(Debug, Clone, Copy)]
    pub enum Untouched {}
}

pub struct Router<RS, R: Route<RS>, S = (), HasRoute = private::Untouched> {
    state: S,
    route: R,
    _priv: PhantomData<(RS, HasRoute)>,
}

impl<RS> Router<RS, route::NotFound> {
    pub fn new() -> Self {
        Self {
            state: (),
            route: route::NotFound,
            _priv: Default::default(),
        }
    }
}

impl<RS> Default for Router<RS, route::NotFound> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R, S> Router<(), R, S, private::Untouched>
where
    R: Route<()>,
{
    pub fn with_state<S2>(self, state: S2) -> Router<S2, R, S2, private::HasAnyState>
    where
        R: Route<S2>,
    {
        Router {
            route: self.route,
            state,
            _priv: Default::default(),
        }
    }
}

impl<RS, R, S> Router<RS, R, S, private::HasAnyState>
where
    R: Route<RS>,
{
    pub fn with_state<S2>(self, state: S2) -> Router<S2, R, S2, private::HasAnyState>
    where
        R: Route<S2>,
    {
        Router {
            route: self.route,
            state,
            _priv: Default::default(),
        }
    }
}

macro_rules! impl_method {
    ($method:ident) => {
        impl<RS, R, S, HasRoute> Router<RS, R, S, HasRoute>
        where
            R: Route<RS>,
        {
            pub fn $method<H, X>(
                self,
                path: &'static str,
                handler: H,
            ) -> Router<RS, impl Route<RS>, S, private::HasAnyState>
            where
                H: handler::HandlerFunction<RS, X>,
            {
                self.route(path, route::$method(handler))
            }
        }
    };
}

impl_method!(get);
impl_method!(post);
impl_method!(put);
impl_method!(delete);
impl_method!(head);
impl_method!(options);
impl_method!(connect);
impl_method!(patch);
impl_method!(trace);

impl<RS, R, S, HasRoute> Router<RS, R, S, HasRoute>
where
    R: Route<RS>,
{
    pub fn route<T: Route<RS>>(
        self,
        path: &'static str,
        route: T,
    ) -> Router<RS, impl Route<RS>, S, private::HasAnyState> {
        Router {
            route: route::Fallback {
                route: route::Path { path, route },
                fallback: self.route,
            },
            state: self.state,
            _priv: Default::default(),
        }
    }
}

impl<R: Route<S> + 'static, S, HasRoute> Service for Router<S, R, S, HasRoute> {
    type BodyError = <<R::Response as IntoResponse>::Body as ErrorType>::Error;

    async fn serve<Re: Read, Wr: Write<Error = Re::Error>>(
        &self,
        mut reader: Re,
        mut writer: Wr,
    ) -> Result<(), ServiceError<Re::Error, Self::BodyError>> {
        // TODO: buf size, optinally make the buffer an arg
        let mut buf = [0u8; 2048];

        const MAX_HEADERS: usize = 100;

        let mut headers_indices: [MaybeUninit<HeaderIndices>; MAX_HEADERS] = unsafe {
            // SAFETY: We can go safely from MaybeUninit array to array of MaybeUninit
            MaybeUninit::uninit().assume_init()
        };

        let mut pos = 0;
        let (method, path, headers, body_start) = loop {
            // TODO check if buffer is full first
            let read = reader
                .read(&mut buf[pos..])
                .await
                .map_err(ServiceError::Io)?;
            if read == 0 {
                // TODO
                return Ok(());
            }
            pos += read;

            let mut headers: [MaybeUninit<httparse::Header<'_>>; MAX_HEADERS] =
                unsafe { MaybeUninit::uninit().assume_init() };
            let mut req = httparse::Request::new(&mut []);

            match req.parse_with_uninit_headers(&buf, &mut headers) {
                Ok(httparse::Status::Complete(len)) => {
                    record_header_indices(&buf, req.headers, &mut headers_indices);

                    let headers = unsafe {
                        MaybeUninit::slice_assume_init_ref(&headers_indices[..req.headers.len()])
                    };

                    // TODO: I think these unwraps cant happen, double check
                    break (req.method.unwrap(), req.path.unwrap(), headers, len);
                }
                Ok(httparse::Status::Partial) => {
                    continue;
                }
                Err(err) => return Err(ServiceError::ProtocolError(ProtocolError::Parser(err))),
            }
        };

        let paq = PathAndQuery::parse(path)
            .map_err(ProtocolError::InvalidUrl)
            .map_err(ServiceError::ProtocolError)?;
        let parts = Parts {
            method: Method::new(method)
                .map_err(ProtocolError::InvalidMethod)
                .map_err(ServiceError::ProtocolError)?,
            path: paq.path(),
            query: paq.query(),
            headers: Headers { headers, buf: &buf },
        };

        let content_length = parts
            .headers
            .get_first("Content-Length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);

        let body = Body::new(content_length, &buf[body_start..pos], reader);
        let request = Request::from_parts(parts, body);

        let response = self
            .route
            .match_request(request, &self.state)
            .await
            // It is safe to unwrap here, we always have a `NotFound` fallback handler.
            .unwrap()
            .into_response();

        use utils::{WriteExt, WriteFmtError};
        write!(writer, "HTTP/1.1 {}\r\n", response.status_code())
            .await
            .map_err(|err| match err {
                WriteFmtError::FmtError => unreachable!("internal format buffer too small"),
                WriteFmtError::Other(err) => ServiceError::Io(err),
            })?;
        writer.write_all(b"\r\n").await.map_err(ServiceError::Io)?;

        let mut body = response.into_body();
        loop {
            let mut buf = [0; 1024];
            let len = body.read(&mut buf).await.map_err(ServiceError::Body)?;
            if len == 0 {
                break;
            }
            writer
                .write_all(&buf[..len])
                .await
                .map_err(ServiceError::Io)?;
        }

        Ok(())
    }
}
