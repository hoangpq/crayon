use std::str;
use std::cell::{Cell, RefCell};
use std::borrow::Borrow;
use std::collections::HashMap;

use gl;
use gl::types::*;

use utils::Handle;
use graphics::*;

use super::*;
use super::visitor::*;

use super::super::frame::{TaskBuffer, TaskBufferPtr};

type ResourceID = GLuint;

#[derive(Debug, Clone, Copy)]
struct VertexBufferObject {
    id: ResourceID,
    setup: VertexBufferSetup,
}

#[derive(Debug, Clone, Copy)]
struct IndexBufferObject {
    id: ResourceID,
    setup: IndexBufferSetup,
}

#[derive(Debug)]
struct PipelineStateObject {
    id: ResourceID,
    setup: PipelineStateSetup,
    uniforms: HashMap<String, UniformVariable>,
}

#[derive(Debug, Clone)]
struct ViewStateObject {
    drawcalls: RefCell<Vec<DrawCall>>,
    setup: ViewStateSetup,
}

#[derive(Debug, Copy, Clone)]
enum GenericTextureSetup {
    Normal(TextureSetup),
    Render(RenderTextureSetup),
}

#[derive(Debug, Copy, Clone)]
struct TextureObject {
    id: ResourceID,
    setup: GenericTextureSetup,
}

#[derive(Debug, Copy, Clone)]
struct RenderBufferObject {
    id: ResourceID,
    setup: RenderBufferSetup,
}

#[derive(Debug, Copy, Clone)]
struct FrameBufferObject {
    id: ResourceID,
}

#[derive(Debug, Copy, Clone)]
struct DrawCall {
    priority: u64,
    view: ViewStateHandle,
    pipeline: PipelineStateHandle,
    uniforms: TaskBufferPtr<[(TaskBufferPtr<str>, UniformVariable)]>,
    textures: TaskBufferPtr<[(TaskBufferPtr<str>, TextureHandle)]>,
    vb: VertexBufferHandle,
    ib: Option<IndexBufferHandle>,
    primitive: Primitive,
    from: u32,
    len: u32,
}

pub struct Device {
    visitor: OpenGLVisitor,

    vertex_buffers: DataVec<VertexBufferObject>,
    index_buffers: DataVec<IndexBufferObject>,
    pipelines: DataVec<PipelineStateObject>,
    views: DataVec<ViewStateObject>,
    textures: DataVec<TextureObject>,
    render_buffers: DataVec<RenderBufferObject>,
    framebuffers: DataVec<FrameBufferObject>,

    active_pipeline: Cell<Option<PipelineStateHandle>>,
}

unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    pub unsafe fn new() -> Self {
        Device {
            visitor: OpenGLVisitor::new(),
            vertex_buffers: DataVec::new(),
            index_buffers: DataVec::new(),
            pipelines: DataVec::new(),
            views: DataVec::new(),
            textures: DataVec::new(),
            render_buffers: DataVec::new(),
            framebuffers: DataVec::new(),
            active_pipeline: Cell::new(None),
        }
    }
}

impl Device {
    pub unsafe fn run_one_frame(&self) -> Result<()> {
        for v in self.views.buf.iter() {
            if let Some(vo) = v.as_ref() {
                vo.drawcalls.borrow_mut().clear();
            }
        }

        self.active_pipeline.set(None);
        self.visitor.bind_framebuffer(0, false)?;
        Ok(())
    }

    pub fn submit(&self,
                  priority: u64,
                  view: ViewStateHandle,
                  pipeline: PipelineStateHandle,
                  textures: TaskBufferPtr<[(TaskBufferPtr<str>, TextureHandle)]>,
                  uniforms: TaskBufferPtr<[(TaskBufferPtr<str>, UniformVariable)]>,
                  vb: VertexBufferHandle,
                  ib: Option<IndexBufferHandle>,
                  primitive: Primitive,
                  from: u32,
                  len: u32)
                  -> Result<()> {
        if let Some(vo) = self.views.get(view) {
            vo.drawcalls
                .borrow_mut()
                .push(DrawCall {
                          priority: priority,
                          view: view,
                          pipeline: pipeline,
                          textures: textures,
                          uniforms: uniforms,
                          vb: vb,
                          ib: ib,
                          primitive: primitive,
                          from: from,
                          len: len,
                      });
            Ok(())
        } else {
            bail!(ErrorKind::InvalidHandle);
        }
    }

    pub unsafe fn flush(&self, buf: &TaskBuffer, dimensions: (u32, u32)) -> Result<()> {
        // Collects avaiable views.
        let (mut views, mut ordered_views) = (vec![], vec![]);
        for (i, v) in self.views.buf.iter().enumerate() {
            if let Some(vo) = v.as_ref() {
                if vo.setup.order == 0 {
                    views.push(i);
                } else {
                    ordered_views.push(i);
                }
            }
        }

        // Sort views by user defined priorities.
        ordered_views.sort_by(|lhs, rhs| {
                                  let lv = self.views.buf[*lhs].as_ref().unwrap();
                                  let rv = self.views.buf[*rhs].as_ref().unwrap();
                                  rv.setup.order.cmp(&lv.setup.order)
                              });

        let mut uniforms = vec![];
        let mut textures = vec![];
        ordered_views.append(&mut views);

        let dimensions = (dimensions.0 as u16, dimensions.1 as u16);
        for i in ordered_views {
            let vo = self.views.buf[i].as_ref().unwrap();

            // Bind frame buffer and clear it.
            if let Some(fbo) = vo.setup.framebuffer {
                if let Some(fbo) = self.framebuffers.get(fbo) {
                    self.visitor.bind_framebuffer(fbo.id, true)?;
                } else {
                    bail!(ErrorKind::InvalidHandle);
                }
            } else {
                self.visitor.bind_framebuffer(0, false)?;
            }

            // Clear frame buffer.
            self.visitor
                .clear(vo.setup.clear_color,
                       vo.setup.clear_depth,
                       vo.setup.clear_stencil)?;

            // Bind the viewport.
            let vp = vo.setup.viewport;
            self.visitor.set_viewport(vp.0, vp.1.unwrap_or(dimensions))?;

            // Sort bucket drawcalls.
            if !vo.setup.sequence {
                vo.drawcalls
                    .borrow_mut()
                    .sort_by(|lhs, rhs| rhs.priority.cmp(&lhs.priority));
            }

            // Submit real OpenGL drawcall in order.
            for dc in vo.drawcalls.borrow().iter() {
                uniforms.clear();
                for &(name, variable) in buf.as_slice(dc.uniforms) {
                    let name = buf.as_str(name);
                    uniforms.push((name, variable));
                }

                textures.clear();
                for &(name, texture) in buf.as_slice(dc.textures) {
                    let name = buf.as_str(name);
                    textures.push((name, texture));
                }

                // Bind program and associated uniforms and textures.
                let pso = self.bind_pipeline(dc.pipeline)?;

                for &(name, variable) in &uniforms {
                    let location = self.visitor.get_uniform_location(pso.id, &name)?;
                    if location == -1 {
                        bail!(format!("failed to locate uniform {}.", &name));
                    }
                    self.visitor.bind_uniform(location, &variable)?;
                }

                for (i, &(name, texture)) in textures.iter().enumerate() {
                    if let Some(to) = self.textures.get(texture) {
                        let location = self.visitor.get_uniform_location(pso.id, &name)?;
                        if location == -1 {
                            bail!(format!("failed to locate texture {}.", &name));
                        }

                        self.visitor
                            .bind_uniform(location, &UniformVariable::I32(i as i32))?;
                        self.visitor.bind_texture(i as u32, to.id)?;
                    } else {
                        bail!(format!("use invalid texture handle {:?} at {}", texture, name));
                    }
                }

                // Bind vertex buffer and vertex array object.
                let vbo = self.vertex_buffers
                    .get(dc.vb)
                    .ok_or(ErrorKind::InvalidHandle)?;
                self.visitor.bind_buffer(gl::ARRAY_BUFFER, vbo.id)?;
                self.visitor
                    .bind_attribute_layout(&pso.setup.layout, &vbo.setup.layout)?;

                // Bind index buffer object if available.
                if let Some(v) = dc.ib {
                    if let Some(ibo) = self.index_buffers.get(v) {
                        gl::DrawElements(dc.primitive.into(),
                                         dc.len as GLsizei,
                                         ibo.setup.format.into(),
                                         dc.from as *const u32 as *const ::std::os::raw::c_void);
                    } else {
                        bail!(ErrorKind::InvalidHandle);
                    }
                } else {
                    gl::DrawArrays(dc.primitive.into(), dc.from as i32, dc.len as i32);
                }

                check()?;
            }
        }

        Ok(())
    }

    unsafe fn bind_pipeline(&self, pipeline: PipelineStateHandle) -> Result<&PipelineStateObject> {
        let pso = self.pipelines
            .get(pipeline)
            .ok_or(ErrorKind::InvalidHandle)?;

        if let Some(v) = self.active_pipeline.get() {
            if v == pipeline {
                return Ok(&pso);
            }
        }

        self.visitor.bind_program(pso.id)?;

        let state = &pso.setup.state;
        self.visitor.set_cull_face(state.cull_face)?;
        self.visitor.set_front_face_order(state.front_face_order)?;
        self.visitor.set_depth_test(state.depth_test)?;
        self.visitor
            .set_depth_write(state.depth_write, state.depth_write_offset)?;
        self.visitor.set_color_blend(state.color_blend)?;

        let c = &state.color_write;
        self.visitor.set_color_write(c.0, c.1, c.2, c.3)?;

        for (name, variable) in &pso.uniforms {
            let location = self.visitor.get_uniform_location(pso.id, &name)?;
            if location != -1 {
                self.visitor.bind_uniform(location, &variable)?;
            }
        }

        self.active_pipeline.set(Some(pipeline));
        Ok(&pso)
    }
}

impl Device {
    pub unsafe fn create_vertex_buffer(&mut self,
                                       handle: VertexBufferHandle,
                                       setup: VertexBufferSetup,
                                       data: Option<&[u8]>)
                                       -> Result<()> {
        if self.vertex_buffers.get(handle).is_some() {
            bail!(ErrorKind::DuplicatedHandle)
        }

        let vbo = VertexBufferObject {
            id: self.visitor
                .create_buffer(OpenGLBuffer::Vertex, setup.hint, setup.len() as u32, data)?,
            setup: setup,
        };

        self.vertex_buffers.set(handle, vbo);
        check()
    }

    pub unsafe fn update_vertex_buffer(&mut self,
                                       handle: VertexBufferHandle,
                                       offset: usize,
                                       data: &[u8])
                                       -> Result<()> {
        if let Some(vbo) = self.vertex_buffers.get(handle) {
            if vbo.setup.hint == BufferHint::Immutable {
                bail!(ErrorKind::InvalidUpdateStaticResource);
            }

            if data.len() + offset > vbo.setup.len() {
                bail!(ErrorKind::OutOfBounds);
            }

            self.visitor
                .update_buffer(vbo.id, OpenGLBuffer::Vertex, offset as u32, data)
        } else {
            bail!(ErrorKind::InvalidHandle);
        }
    }

    pub unsafe fn delete_vertex_buffer(&mut self, handle: VertexBufferHandle) -> Result<()> {
        if let Some(vbo) = self.vertex_buffers.remove(handle) {
            self.visitor.delete_buffer(vbo.id)
        } else {
            bail!(ErrorKind::InvalidHandle);
        }
    }

    pub unsafe fn create_index_buffer(&mut self,
                                      handle: IndexBufferHandle,
                                      setup: IndexBufferSetup,
                                      data: Option<&[u8]>)
                                      -> Result<()> {
        if self.index_buffers.get(handle).is_some() {
            bail!(ErrorKind::DuplicatedHandle)
        }

        let ibo = IndexBufferObject {
            id: self.visitor
                .create_buffer(OpenGLBuffer::Index, setup.hint, setup.len() as u32, data)?,
            setup: setup,
        };

        self.index_buffers.set(handle, ibo);
        check()
    }

    pub unsafe fn update_index_buffer(&mut self,
                                      handle: IndexBufferHandle,
                                      offset: usize,
                                      data: &[u8])
                                      -> Result<()> {
        if let Some(ibo) = self.index_buffers.get(handle) {
            if ibo.setup.hint == BufferHint::Immutable {
                bail!(ErrorKind::InvalidUpdateStaticResource);
            }

            if data.len() + offset > ibo.setup.len() {
                bail!(ErrorKind::OutOfBounds);
            }

            self.visitor
                .update_buffer(ibo.id, OpenGLBuffer::Index, offset as u32, data)
        } else {
            bail!(ErrorKind::InvalidHandle);
        }
    }

    pub unsafe fn delete_index_buffer(&mut self, handle: IndexBufferHandle) -> Result<()> {
        if let Some(ibo) = self.index_buffers.remove(handle) {
            self.visitor.delete_buffer(ibo.id)
        } else {
            bail!(ErrorKind::InvalidHandle);
        }
    }

    pub unsafe fn create_render_buffer(&mut self,
                                       handle: RenderBufferHandle,
                                       setup: RenderBufferSetup)
                                       -> Result<()> {
        let (internal_format, _, _) = setup.format.into();
        let id =
            self.visitor
                .create_render_buffer(internal_format, setup.dimensions.0, setup.dimensions.1)?;

        self.render_buffers
            .set(handle,
                 RenderBufferObject {
                     id: id,
                     setup: setup,
                 });
        Ok(())
    }

    pub unsafe fn delete_render_buffer(&mut self, handle: RenderBufferHandle) -> Result<()> {
        if let Some(rto) = self.render_buffers.remove(handle) {
            self.visitor.delete_render_buffer(rto.id)
        } else {
            bail!(ErrorKind::InvalidHandle);
        }
    }

    pub unsafe fn create_framebuffer(&mut self, handle: FrameBufferHandle) -> Result<()> {
        if self.framebuffers.get(handle).is_some() {
            bail!(ErrorKind::DuplicatedHandle)
        }

        let fbo = FrameBufferObject { id: self.visitor.create_framebuffer()? };

        self.framebuffers.set(handle, fbo);
        Ok(())
    }

    pub unsafe fn update_framebuffer_with_texture(&mut self,
                                                  handle: FrameBufferHandle,
                                                  texture: TextureHandle,
                                                  slot: u32)
                                                  -> Result<()> {
        let fbo = self.framebuffers
            .get(handle)
            .ok_or(ErrorKind::InvalidHandle)?;

        let texture = self.textures.get(texture).ok_or(ErrorKind::InvalidHandle)?;
        if let GenericTextureSetup::Render(setup) = texture.setup {
            self.visitor.bind_framebuffer(fbo.id, false)?;
            match setup.format {
                RenderTextureFormat::RGB8 |
                RenderTextureFormat::RGBA4 |
                RenderTextureFormat::RGBA8 => {
                    let location = gl::COLOR_ATTACHMENT0 + slot;
                    self.visitor
                        .bind_framebuffer_with_texture(location, texture.id)
                }
                RenderTextureFormat::Depth16 |
                RenderTextureFormat::Depth24 |
                RenderTextureFormat::Depth32 => {
                    self.visitor
                        .bind_framebuffer_with_texture(gl::DEPTH_ATTACHMENT, texture.id)
                }
                RenderTextureFormat::Depth24Stencil8 => {
                    self.visitor
                        .bind_framebuffer_with_texture(gl::DEPTH_STENCIL_ATTACHMENT, texture.id)
                }
            }
        } else {
            bail!("can't attach normal texture to framebuffer.");
        }
    }

    pub unsafe fn update_framebuffer_with_renderbuffer(&mut self,
                                                       handle: FrameBufferHandle,
                                                       buf: RenderBufferHandle,
                                                       slot: u32)
                                                       -> Result<()> {
        let fbo = self.framebuffers
            .get(handle)
            .ok_or(ErrorKind::InvalidHandle)?;
        let buf = self.render_buffers
            .get(buf)
            .ok_or(ErrorKind::InvalidHandle)?;

        self.visitor.bind_framebuffer(fbo.id, false)?;
        match buf.setup.format {
            RenderTextureFormat::RGB8 |
            RenderTextureFormat::RGBA4 |
            RenderTextureFormat::RGBA8 => {
                let location = gl::COLOR_ATTACHMENT0 + slot;
                self.visitor
                    .bind_framebuffer_with_renderbuffer(location, buf.id)
            }
            RenderTextureFormat::Depth16 |
            RenderTextureFormat::Depth24 |
            RenderTextureFormat::Depth32 => {
                self.visitor
                    .bind_framebuffer_with_renderbuffer(gl::DEPTH_ATTACHMENT, buf.id)
            }
            RenderTextureFormat::Depth24Stencil8 => {
                self.visitor
                    .bind_framebuffer_with_renderbuffer(gl::DEPTH_STENCIL_ATTACHMENT, buf.id)
            }
        }
    }

    pub unsafe fn delete_framebuffer(&mut self, handle: FrameBufferHandle) -> Result<()> {
        if let Some(fbo) = self.framebuffers.remove(handle) {
            self.visitor.delete_framebuffer(fbo.id)
        } else {
            bail!(ErrorKind::InvalidHandle);
        }
    }

    pub unsafe fn create_render_texture(&mut self,
                                        handle: TextureHandle,
                                        setup: RenderTextureSetup)
                                        -> Result<()> {
        let (internal_format, in_format, pixel_type) = setup.format.into();
        let id = self.visitor
            .create_texture(internal_format,
                            in_format,
                            pixel_type,
                            TextureAddress::Repeat,
                            TextureFilter::Linear,
                            false,
                            setup.dimensions.0,
                            setup.dimensions.1,
                            None)?;

        self.textures
            .set(handle,
                 TextureObject {
                     id: id,
                     setup: GenericTextureSetup::Render(setup),
                 });
        Ok(())
    }

    pub unsafe fn create_texture(&mut self,
                                 handle: TextureHandle,
                                 setup: TextureSetup,
                                 data: Vec<u8>)
                                 -> Result<()> {
        let (internal_format, in_format, pixel_type) = setup.format.into();
        let id = self.visitor
            .create_texture(internal_format,
                            in_format,
                            pixel_type,
                            setup.address,
                            setup.filter,
                            setup.mipmap,
                            setup.dimensions.0,
                            setup.dimensions.1,
                            Some(&data))?;

        self.textures
            .set(handle,
                 TextureObject {
                     id: id,
                     setup: GenericTextureSetup::Normal(setup),
                 });
        Ok(())
    }

    pub unsafe fn delete_texture(&mut self, handle: TextureHandle) -> Result<()> {
        if let Some(texture) = self.textures.remove(handle) {
            self.visitor.delete_texture(texture.id)?;
            Ok(())
        } else {
            bail!(ErrorKind::InvalidHandle);
        }
    }

    pub fn create_view(&mut self, handle: ViewStateHandle, setup: ViewStateSetup) -> Result<()> {
        let view = ViewStateObject {
            drawcalls: RefCell::new(Vec::new()),
            setup: setup,
        };

        self.views.set(handle, view);
        Ok(())
    }

    pub fn delete_view(&mut self, handle: ViewStateHandle) -> Result<()> {
        if let Some(_) = self.views.remove(handle) {
            Ok(())
        } else {
            bail!(ErrorKind::InvalidHandle);
        }
    }

    /// Initializes named program object. A program object is an object to
    /// which shader objects can be attached. Vertex and fragment shader
    /// are minimal requirement to build a proper program.
    pub unsafe fn create_pipeline(&mut self,
                                  handle: PipelineStateHandle,
                                  setup: PipelineStateSetup,
                                  vs_src: String,
                                  fs_src: String)
                                  -> Result<()> {

        let pid = self.visitor.create_program(&vs_src, &fs_src)?;

        for (name, _) in setup.layout.iter() {
            let name: &'static str = name.into();
            let location = self.visitor.get_attribute_location(pid, name)?;
            if location == -1 {
                bail!(format!("failed to locate attribute {:?}", name));
            }
        }

        self.pipelines
            .set(handle,
                 PipelineStateObject {
                     id: pid,
                     setup: setup,
                     uniforms: HashMap::new(),
                 });
        check()
    }

    pub fn update_pipeline_uniform(&mut self,
                                   handle: PipelineStateHandle,
                                   name: &str,
                                   variable: &UniformVariable)
                                   -> Result<()> {
        if let Some(pso) = self.pipelines.get_mut(handle) {
            pso.uniforms.insert(name.to_string(), *variable);
            Ok(())
        } else {
            bail!(ErrorKind::InvalidHandle);
        }
    }

    /// Free named program object.
    pub unsafe fn delete_pipeline(&mut self, handle: PipelineStateHandle) -> Result<()> {
        if let Some(pso) = self.pipelines.remove(handle) {
            self.visitor.delete_program(pso.id)
        } else {
            bail!(ErrorKind::InvalidHandle);
        }
    }
}

struct DataVec<T>
    where T: Sized
{
    pub buf: Vec<Option<T>>,
}

impl<T> DataVec<T>
    where T: Sized
{
    pub fn new() -> Self {
        DataVec { buf: Vec::new() }
    }

    pub fn get<H>(&self, handle: H) -> Option<&T>
        where H: Borrow<Handle>
    {
        self.buf
            .get(handle.borrow().index() as usize)
            .and_then(|v| v.as_ref())
    }

    pub fn get_mut<H>(&mut self, handle: H) -> Option<&mut T>
        where H: Borrow<Handle>
    {
        self.buf
            .get_mut(handle.borrow().index() as usize)
            .and_then(|v| v.as_mut())
    }

    pub fn set<H>(&mut self, handle: H, value: T)
        where H: Borrow<Handle>
    {
        let handle = handle.borrow();
        while self.buf.len() <= handle.index() as usize {
            self.buf.push(None);
        }

        self.buf[handle.index() as usize] = Some(value);
    }

    pub fn remove<H>(&mut self, handle: H) -> Option<T>
        where H: Borrow<Handle>
    {
        let handle = handle.borrow();
        if self.buf.len() <= handle.index() as usize {
            None
        } else {
            let mut value = None;
            ::std::mem::swap(&mut value, &mut self.buf[handle.index() as usize]);
            value
        }
    }
}