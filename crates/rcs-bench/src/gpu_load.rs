//! A stand-in for a game in `rcs-bench load --kind gpu`: a full-screen pixel shader with a long math loop, drawn
//! without a frame cap, so the 3D engine of the GPU is busy all the time. It is not a game: no textures, no geometry,
//! no CPU work. It shows how capture and encoding behave when the GPU has no idle time left.

use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
    ID3DBlob,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION,
    D3D11_USAGE_DEFAULT, D3D11_VIEWPORT, D3D11CreateDeviceAndSwapChain, ID3D11Buffer,
    ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11Texture2D,
    ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_DESC, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT, DXGI_PRESENT_ALLOW_TEARING, DXGI_SWAP_CHAIN_DESC,
    DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGISwapChain,
};
use windows::core::{PCSTR, s};

/// Iterations of the per-pixel loop. Without a frame cap the GPU is busy anyway; this keeps one frame long enough
/// that the load does not turn into a present-rate test.
const ITERATIONS: u32 = 96;

const SHADER: &str = r#"
cbuffer Frame : register(b0) { float time; float iterations; float2 size; };

float4 vs(uint id : SV_VertexID) : SV_Position {
    float2 p = float2((id << 1) & 2, id & 2);
    return float4(p * float2(2, -2) + float2(-1, 1), 0, 1);
}

float4 ps(float4 pos : SV_Position) : SV_Target {
    float2 uv = pos.xy / size;
    float3 c = float3(uv, 0.5);
    [loop] for (int i = 0; i < (int)iterations; i++) {
        c = 0.5 + 0.5 * sin(float3(uv * (i + 1.0), time + i * 0.1) * 3.1 + c.yzx * 2.0);
    }
    return float4(c, 1);
}
"#;

#[repr(C)]
struct FrameConstants {
    time: f32,
    iterations: f32,
    size: [f32; 2],
}

pub struct GpuRenderer {
    swap_chain: IDXGISwapChain,
    context: ID3D11DeviceContext,
    target: ID3D11RenderTargetView,
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    constants: ID3D11Buffer,
    width: u32,
    tearing: bool,
    height: u32,
}

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob, String> {
    let mut code = None;
    let mut errors = None;
    // SAFETY: the source is a live string with its length; the out pointers are live locals.
    let result = unsafe {
        D3DCompile(
            SHADER.as_ptr().cast(),
            SHADER.len(),
            s!("gpu_load.hlsl"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = result {
        let detail = errors
            .map(|blob: ID3DBlob| {
                // SAFETY: the blob holds the compiler message of the given size.
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        blob.GetBufferPointer() as *const u8,
                        blob.GetBufferSize(),
                    )
                };
                String::from_utf8_lossy(bytes).into_owned()
            })
            .unwrap_or_default();
        return Err(format!("shader: {e} {detail}"));
    }
    code.ok_or_else(|| "the compiler returned no code".to_string())
}

fn bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: the blob owns this many bytes for its lifetime, which bounds the slice.
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    }
}

impl GpuRenderer {
    pub fn new(hwnd: HWND, width: u32, height: u32) -> Result<Self, String> {
        // With tearing allowed, Present(0) does not wait for the display, like a game without vsync.
        match Self::create(hwnd, width, height, true) {
            Ok(renderer) => Ok(renderer),
            Err(_) => Self::create(hwnd, width, height, false),
        }
    }

    /// Whether frames are presented without waiting for the display.
    pub fn tearing(&self) -> bool {
        self.tearing
    }

    fn create(hwnd: HWND, width: u32, height: u32, tearing: bool) -> Result<Self, String> {
        let desc = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                ..Default::default()
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            OutputWindow: hwnd,
            Windowed: true.into(),
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            Flags: if tearing {
                DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32
            } else {
                0
            },
        };
        let mut swap_chain = None;
        let mut device = None;
        let mut context = None;
        // SAFETY: the descriptor and all out pointers are live locals.
        unsafe {
            D3D11CreateDeviceAndSwapChain(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&desc),
                Some(&mut swap_chain),
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(|e| format!("device and swap chain: {e}"))?;
        let (swap_chain, device, context) = match (swap_chain, device, context) {
            (Some(s), Some(d), Some(c)) => (s, d, c),
            _ => return Err("D3D11CreateDeviceAndSwapChain returned nothing".into()),
        };

        let vs_code = compile(s!("vs"), s!("vs_5_0"))?;
        let ps_code = compile(s!("ps"), s!("ps_5_0"))?;
        let mut vertex = None;
        let mut pixel = None;
        let mut target = None;
        let mut constants = None;
        let buffer_desc = D3D11_BUFFER_DESC {
            ByteWidth: size_of::<FrameConstants>() as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        // SAFETY: COM calls on the new device; bytecode slices and out pointers are live for each call.
        unsafe {
            device
                .CreateVertexShader(bytes(&vs_code), None, Some(&mut vertex))
                .map_err(|e| format!("vertex shader: {e}"))?;
            device
                .CreatePixelShader(bytes(&ps_code), None, Some(&mut pixel))
                .map_err(|e| format!("pixel shader: {e}"))?;
            let back: ID3D11Texture2D = swap_chain
                .GetBuffer(0)
                .map_err(|e| format!("back buffer: {e}"))?;
            device
                .CreateRenderTargetView(&back, None, Some(&mut target))
                .map_err(|e| format!("render target: {e}"))?;
            device
                .CreateBuffer(&buffer_desc, None, Some(&mut constants))
                .map_err(|e| format!("constant buffer: {e}"))?;
        }
        Ok(Self {
            swap_chain,
            context,
            target: target.ok_or("no render target")?,
            vertex: vertex.ok_or("no vertex shader")?,
            pixel: pixel.ok_or("no pixel shader")?,
            constants: constants.ok_or("no constant buffer")?,
            width,
            tearing,
            height,
        })
    }

    /// Draws one frame and presents it without waiting for the display.
    pub fn render(&mut self, time: f32) -> Result<(), String> {
        let constants = FrameConstants {
            time,
            iterations: ITERATIONS as f32,
            size: [self.width as f32, self.height as f32],
        };
        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: self.width as f32,
            Height: self.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        // SAFETY: every object belongs to this renderer's device; the constants are a live local.
        unsafe {
            let c = &self.context;
            c.UpdateSubresource(
                &self.constants,
                0,
                None,
                (&constants as *const FrameConstants).cast(),
                0,
                0,
            );
            c.OMSetRenderTargets(Some(&[Some(self.target.clone())]), None);
            c.RSSetViewports(Some(&[viewport]));
            c.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            c.VSSetShader(&self.vertex, None);
            c.PSSetShader(&self.pixel, None);
            c.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            c.Draw(3, 0);
            self.swap_chain
                .Present(
                    0,
                    if self.tearing {
                        DXGI_PRESENT_ALLOW_TEARING
                    } else {
                        DXGI_PRESENT(0)
                    },
                )
                .ok()
                .map_err(|e| format!("Present: {e}"))
        }
    }
}

pub const SHADER_ITERATIONS: u32 = ITERATIONS;
