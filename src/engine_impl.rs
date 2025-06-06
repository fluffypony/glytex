use std::any::Any;

use crate::{
    context_impl::ContextImpl,
    function_impl::FunctionImpl,
    gpu_status_file::{GpuDevice, GpuStatus},
    multi_engine_wrapper::EngineType,
};

pub trait EngineImpl {
    type Context: Any;
    type Function: Any;
    type Kernel: Any;

    fn get_engine_type(&self) -> EngineType;

    fn init(&mut self) -> Result<(), anyhow::Error>;

    fn num_devices(&self) -> Result<u32, anyhow::Error>;

    fn detect_devices(&self) -> Result<Vec<GpuDevice>, anyhow::Error>;

    fn create_context(&self, device_index: u32) -> Result<Self::Context, anyhow::Error>;

    fn create_main_function(&self, context: &Self::Context) -> Result<Self::Function, anyhow::Error>;

    fn create_kernel(&self, function: &Self::Function) -> Result<Self::Kernel, anyhow::Error>;

    fn mine(
        &self,
        kernel: &Self::Kernel,
        func: &Self::Function,
        context: &Self::Context,
        data: &[u64],
        min_difficulty: u64,
        nonce_start: u64,
        num_iterations: u32,
        block_size: u32,
        grid_size: u32,
    ) -> Result<(Option<u64>, u32, u64), anyhow::Error>;
}
