use std::{
    net::SocketAddr,
    sync::{atomic::AtomicBool, Arc},
};

use zingolib::compact_formats::{
    compact_tx_streamer_server::{CompactTxStreamer, CompactTxStreamerServer},
    Address, AddressList, Balance, BlockId, BlockRange, ChainSpec, CompactBlock, CompactTx,
    Duration, Empty, Exclude, GetAddressUtxosArg, GetAddressUtxosReply, GetAddressUtxosReplyList,
    LightdInfo, PingResponse, RawTransaction, SendResponse, TransparentAddressBlockFilter,
    TreeState, TxFilter,
};

macro_rules! define_grpc_passthrough {
    (fn
        $name:ident(
            &$self:ident$(,$($arg:ident: $argty:ty,)*)?
        ) -> $ret:ty
    ) => {
        #[must_use]
        #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
        fn $name<'life0, 'async_trait>(&'life0 $self$($(, $arg: $argty)*)?) ->
           ::core::pin::Pin<Box<
                dyn ::core::future::Future<
                    Output = ::core::result::Result<
                        ::tonic::Response<$ret>,
                        ::tonic::Status
                >
            > + ::core::marker::Send + 'async_trait
        >>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async {
                while !$self.online.load(::core::sync::atomic::Ordering::Relaxed) {
                    ::tokio::time::sleep(::core::time::Duration::from_millis(50)).await;
                }
                println!("Proxy passing through {} call", stringify!($name));
                ::zingolib::grpc_connector::GrpcConnector::new($self.lightwalletd_uri.clone())
                    .get_client()
                    .await
                    .expect("Proxy server failed to create client")
                    .$name($($($arg),*)?)
                    .await
            })
        }
    };
}

pub struct ProxyServer {
    pub lightwalletd_uri: http::Uri,
    pub online: Arc<AtomicBool>,
}

impl ProxyServer {
    pub fn serve(
        self,
        listen_at: SocketAddr,
    ) -> tokio::task::JoinHandle<Result<(), tonic::transport::Error>> {
        tokio::task::spawn(async move {
            let svc = CompactTxStreamerServer::new(self);
            tonic::transport::Server::builder()
                .add_service(svc)
                .serve(listen_at)
                .await
        })
    }
}

impl CompactTxStreamer for ProxyServer {
    define_grpc_passthrough!(
        fn get_latest_block(
            &self,
            request: tonic::Request<ChainSpec>,
        ) -> BlockId
    );

    define_grpc_passthrough!(
        fn get_block(
            &self,
            request: tonic::Request<BlockId>,
        ) -> CompactBlock
    );

    #[doc = "Server streaming response type for the GetBlockRange method."]
    type GetBlockRangeStream = tonic::Streaming<CompactBlock>;

    define_grpc_passthrough!(
        fn get_block_range(
            &self,
            request: tonic::Request<BlockRange>,
        ) -> Self::GetBlockRangeStream
    );

    define_grpc_passthrough!(
        fn get_transaction(
            &self,
            request: tonic::Request<TxFilter>,
        ) -> RawTransaction
    );

    define_grpc_passthrough!(
        fn send_transaction(
            &self,
            request: tonic::Request<RawTransaction>,
        ) -> SendResponse
    );

    #[doc = "Server streaming response type for the GetTaddressTxids method."]
    type GetTaddressTxidsStream = tonic::Streaming<RawTransaction>;

    define_grpc_passthrough!(
        fn get_taddress_txids(
            &self,
            request: tonic::Request<TransparentAddressBlockFilter>,
        ) -> Self::GetTaddressTxidsStream
    );

    define_grpc_passthrough!(
        fn get_taddress_balance(
            &self,
            request: tonic::Request<AddressList>,
        ) -> Balance
    );

    /// This isn't easily definable with the macro, and I beleive it to be unused
    #[must_use]
    #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
    fn get_taddress_balance_stream<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<tonic::Streaming<Address>>,
    ) -> ::core::pin::Pin<
        Box<
            dyn ::core::future::Future<Output = Result<tonic::Response<Balance>, tonic::Status>>
                + ::core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        todo!("this isn't expected to be called. Please implement this if you need it")
    }

    #[doc = "Server streaming response type for the GetMempoolTx method."]
    type GetMempoolTxStream = tonic::Streaming<CompactTx>;

    define_grpc_passthrough!(
        fn get_mempool_tx(
            &self,
            request: tonic::Request<Exclude>,
        ) -> Self::GetMempoolTxStream
    );

    #[doc = "Server streaming response type for the GetMempoolStream method."]
    type GetMempoolStreamStream = tonic::Streaming<RawTransaction>;

    define_grpc_passthrough!(
        fn get_mempool_stream(
            &self,
            request: tonic::Request<Empty>,
        ) -> Self::GetMempoolStreamStream
    );

    define_grpc_passthrough!(
        fn get_tree_state(
            &self,
            request: tonic::Request<BlockId>,
        ) -> TreeState
    );

    define_grpc_passthrough!(
        fn get_address_utxos(
            &self,
            request: tonic::Request<GetAddressUtxosArg>,
        ) -> GetAddressUtxosReplyList
    );

    #[doc = "Server streaming response type for the GetAddressUtxosStream method."]
    type GetAddressUtxosStreamStream = tonic::Streaming<GetAddressUtxosReply>;

    define_grpc_passthrough!(
        fn get_address_utxos_stream(
            &self,
            request: tonic::Request<GetAddressUtxosArg>,
        ) -> tonic::Streaming<GetAddressUtxosReply>
    );

    define_grpc_passthrough!(
        fn get_lightd_info(
            &self,
            request: tonic::Request<Empty>,
        ) -> LightdInfo
    );

    define_grpc_passthrough!(
        fn ping(
            &self,
            request: tonic::Request<Duration>,
        ) -> PingResponse
    );
}
