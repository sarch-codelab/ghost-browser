use crate::browser::{Browser, CHROME_TOP_INSET, Panel, STATUS_HEIGHT, UI_HEIGHT};
use crate::store::BrowserEngine;
use egui::{Align2, Color32, FontId, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};

/// Ancho de cada control de ventana (minimizar, maximizar, cerrar).
const WINDOW_CONTROL_WIDTH: f32 = 46.0;
/// Ancho de la marca (logotipo + nombre) y versión compacta (solo logotipo).
const BRAND_WIDTH: f32 = 86.0;
const BRAND_COMPACT_WIDTH: f32 = 28.0;
/// Anchura del botón «+» de nueva pestaña.
const NEW_TAB_WIDTH: f32 = 26.0;
/// Separación entre las dos filas del chrome.
const ROW_GAP: f32 = 4.0;
/// Separación horizontal entre las secciones de una fila.
const SECTION_GAP: f32 = 6.0;
/// Anchura de un botón de la barra de herramientas y separación entre botones.
const TOOL_BUTTON: f32 = 30.0;
const TOOL_SPACING: f32 = 2.0;
/// Ancho de la fila interna de la barra de direcciones (icono + campo).
const ADDR_ROW_HEIGHT: f32 = 20.0;
/// Por debajo de este ancho de ventana la fila superior se simplifica.
const NARROW_CHROME: f32 = 560.0;
/// Por debajo de este ancho de la segunda fila se ocultan botones secundarios.
const NARROW_ACTIONS: f32 = 520.0;

#[derive(Clone, Copy)]
struct Palette {
    dark: bool,
    bg: Color32,
    chrome: Color32,
    chrome_soft: Color32,
    surface: Color32,
    surface_hi: Color32,
    border: Color32,
    text: Color32,
    muted: Color32,
    muted_strong: Color32,
    accent: Color32,
    tab_active: Color32,
    tab_hover: Color32,
    hover: Color32,
    danger: Color32,
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            dark,
            bg: Color32::from_rgb(0x00, 0x00, 0x00),
            chrome: Color32::from_rgb(0x00, 0x00, 0x00),
            chrome_soft: Color32::from_rgb(0x00, 0x00, 0x00),
            surface: Color32::from_rgb(0x0E, 0x0E, 0x0E),
            surface_hi: Color32::from_rgb(0x1E, 0x1E, 0x1E),
            border: Color32::from_rgb(0x28, 0x28, 0x28),
            text: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            muted: Color32::from_rgb(0x8C, 0x8C, 0x8C),
            muted_strong: Color32::from_rgb(0xC7, 0xC7, 0xC7),
            accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            tab_active: Color32::from_rgb(0x1A, 0x1A, 0x1A),
            tab_hover: Color32::from_rgb(0x12, 0x12, 0x12),
            hover: Color32::from_rgb(0x22, 0x22, 0x22),
            danger: Color32::from_rgb(0xFF, 0xFF, 0xFF),
        }
    } else {
        Palette {
            dark,
            bg: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            chrome: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            chrome_soft: Color32::from_rgb(0xFA, 0xFA, 0xFA),
            surface: Color32::from_rgb(0xF5, 0xF5, 0xF5),
            surface_hi: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            border: Color32::from_rgb(0xD6, 0xD6, 0xD6),
            text: Color32::from_rgb(0x0A, 0x0A, 0x0A),
            muted: Color32::from_rgb(0x70, 0x70, 0x70),
            muted_strong: Color32::from_rgb(0x40, 0x40, 0x40),
            accent: Color32::from_rgb(0x00, 0x00, 0x00),
            tab_active: Color32::from_rgb(0xEC, 0xEC, 0xEC),
            tab_hover: Color32::from_rgb(0xF4, 0xF4, 0xF4),
            hover: Color32::from_rgb(0xEB, 0xEB, 0xEB),
            danger: Color32::from_rgb(0x00, 0x00, 0x00),
        }
    }
}

/// Dibuja toda la interfaz del navegador (pestañas, barra, paneles, estado).
pub fn draw(ctx: &egui::Context, app: &mut Browser) -> bool {
    let p = palette(app.store.settings.dark_mode);
    apply_theme(ctx, &p);
    let mut close_requested = false;
    ctx.root_ui(|ui| {
        let chrome = egui::Panel::top("ghost_chrome")
            .exact_size(UI_HEIGHT)
            .show_separator_line(false)
            .frame(egui::Frame::NONE.fill(p.chrome).inner_margin(egui::Margin {
                left: 6,
                right: 6,
                top: 0,
                bottom: 2,
            }))
            .show(ui, |ui| {
                let content = ui.max_rect();
                // La banda superior de 6 px queda libre para redimensionar y
                // arrastrar la ventana; las dos filas empiezan debajo.
                let row1 = Rect::from_min_size(
                    pos2(content.left(), content.top() + CHROME_TOP_INSET),
                    vec2(content.width(), 32.0),
                );
                let row2 = Rect::from_min_size(
                    pos2(content.left(), row1.bottom() + ROW_GAP),
                    vec2(content.width(), 32.0),
                );
                ui.painter().hline(
                    content.x_range(),
                    content.bottom() - 0.5,
                    Stroke::new(1.0, p.border),
                );

                close_requested = draw_top_row(ui, app, &p, row1, ctx);
                draw_nav_row(ui, app, &p, row2);
            });
        let viewport = ui.clip_rect();
        app.chrome_height = chrome.response.rect.bottom().max(0.0);

        let status = egui::Panel::bottom("ghost_status")
            .exact_size(STATUS_HEIGHT)
            .show_separator_line(false)
            .frame(egui::Frame::NONE.fill(p.chrome_soft))
            .show(ui, |ui| {
                draw_status_bar(ui.painter(), ui.max_rect(), app, &p);
            });
        app.status_height = (viewport.bottom() - status.response.rect.top()).max(0.0);

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(p.bg))
            .show(ui, |ui| {
                if app.splash {
                    draw_splash(ui, app, &p, ui.max_rect());
                } else if app.active_tab().is_some_and(|tab| tab.is_newtab()) {
                    draw_newtab_page(ui, app, &p);
                } else if app
                    .active_tab()
                    .is_some_and(|tab| tab.engine == BrowserEngine::Wisp)
                {
                    draw_wisp_page(ui, app, &p);
                } else if !app.active_tab().map(|t| t.is_live()).unwrap_or(true)
                    || app.active_tab().is_some_and(|t| app.is_restoring(t.id))
                {
                    draw_restore_overlay(ui, app, &p, ui.max_rect());
                }
            });
    });
    draw_panels(ctx, app, &p);
    close_requested
}

fn draw_wisp_page(ui: &mut Ui, app: &mut Browser, p: &Palette) {
    let Some(tab_id) = app.active_tab().map(|tab| tab.id) else {
        return;
    };
    let width = ui.available_width().max(1.0);
    let needs_layout = app.active_tab().is_some_and(|tab| {
        tab.wisp_html.is_some()
            && tab
                .wisp_layout_width
                .is_none_or(|old_width| (old_width - width).abs() > 0.5)
    });
    if needs_layout {
        // El layout corre en un hilo aparte; la UI sigue mostrando el frame
        // anterior (o un placeholder) hasta que llegue el nuevo LayoutDocument.
        app.request_wisp_layout(tab_id, width);
    }

    ui.horizontal(|ui| {
        ui.label(
            RichText::new("WISP BETA")
                .strong()
                .size(10.0)
                .color(p.accent),
        );
        ui.label(
            RichText::new("HTML5 y CSS responsive; JavaScript limitado.")
                .size(10.0)
                .color(p.muted),
        );
    });
    let warnings = app
        .active_tab()
        .map(|tab| {
            let mut warnings = Vec::new();
            if let Some(page) = tab.wisp_html.as_ref() {
                warnings.extend(page.warnings.iter().cloned());
            }
            if let Some(layout) = tab.wisp_layout.as_ref() {
                warnings.extend(layout.warnings.iter().cloned());
            }
            warnings
        })
        .unwrap_or_default();
    for warning in warnings.iter().take(3) {
        ui.label(RichText::new(warning).size(10.0).color(p.muted));
    }
    if warnings.len() > 3 {
        ui.label(
            RichText::new(format!("y {} advertencias más.", warnings.len() - 3))
                .size(10.0)
                .color(p.muted),
        );
    }
    let needs_webview = warnings_need_webview(&warnings);
    let mut open_in_webview = false;
    if needs_webview {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(
                    "Esta página necesita recursos que Wisp no pudo ejecutar; puede faltar contenido.",
                )
                .size(10.0)
                .color(p.muted),
            );
            if ui.small_button("Abrir con WebView2").clicked() {
                open_in_webview = true;
            }
        });
    }
    ui.separator();

    let tab = app.active_tab();
    let loading = tab.is_some_and(|tab| tab.loading);
    let html_available = tab.is_some_and(|tab| tab.wisp_html.is_some());
    let error = tab.and_then(|tab| tab.wisp_error.clone());
    if loading {
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("Cargando página con Wisp…").color(p.muted));
        });
        return;
    }
    if let Some(error) = error {
        ui.vertical_centered(|ui| {
            ui.add_space(36.0);
            ui.label(
                RichText::new("Wisp no pudo mostrar esta página")
                    .strong()
                    .color(p.text),
            );
            ui.add_space(6.0);
            ui.label(RichText::new(error).size(12.0).color(p.muted));
            ui.add_space(12.0);
            if ui.button("Reintentar").clicked() {
                app.reload();
            }
        });
        return;
    }
    if !html_available {
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("Esta pestaña Wisp aún no tiene contenido.").color(p.muted));
        });
        return;
    }

    let Some(document) = app.active_tab().and_then(|tab| tab.wisp_layout.clone()) else {
        if app.wisp_layout_pending(tab_id) {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Wisp está calculando el layout…").color(p.muted));
            });
        }
        return;
    };
    let active_url = app
        .active_tab()
        .map(|tab| tab.url.clone())
        .unwrap_or_default();
    let mut navigate_to = None;
    egui::ScrollArea::vertical()
        .id_salt(("wisp_page", tab_id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(width);
            let (page_rect, _) = ui.allocate_exact_size(
                vec2(width, document.content_height.max(1.0)),
                Sense::hover(),
            );
            let painter = ui.painter().with_clip_rect(page_rect);
            // Viewport culling: el documento puede tener miles de comandos;
            // solo se envían a la GPU los que intersectan la franja visible.
            // Las coordenadas de los comandos son relativas al documento y
            // `page_rect.min.y` es el origen en pantalla del documento.
            let view = ui.clip_rect();
            let view_top = view.min.y - page_rect.min.y;
            let view_bottom = view.max.y - page_rect.min.y;
            for command in &document.commands {
                let (top, bottom) = command_vspan(command);
                if bottom < view_top || top > view_bottom {
                    continue;
                }
                match command {
                    wisp_engine::PaintCommand::FillRect {
                        rect,
                        color,
                        radius,
                    } => {
                        let rect = Rect::from_min_size(
                            page_rect.min + vec2(rect.x, rect.y),
                            vec2(rect.width, rect.height),
                        );
                        painter.rect_filled(rect, *radius, color32(*color));
                    }
                    wisp_engine::PaintCommand::StrokeRect {
                        rect,
                        color,
                        width,
                        radius,
                    } => {
                        let rect = Rect::from_min_size(
                            page_rect.min + vec2(rect.x, rect.y),
                            vec2(rect.width, rect.height),
                        );
                        painter.rect_stroke(
                            rect,
                            *radius,
                            egui::Stroke::new(*width, color32(*color)),
                            egui::StrokeKind::Inside,
                        );
                    }
                    wisp_engine::PaintCommand::Text {
                        x,
                        y,
                        content,
                        color,
                        font_size,
                        font_weight,
                        href,
                    } => {
                        let text_position = page_rect.min + vec2(*x, *y);
                        let font_id = if *font_weight >= 600 {
                            FontId::new(*font_size, egui::FontFamily::Name("wisp-bold".into()))
                        } else {
                            FontId::proportional(*font_size)
                        };
                        // `:hover`/`:active` de enlaces: la respuesta de egui se
                        // crea antes de pintar para poder cambiar color y
                        // subrayar sin recalcular el layout.
                        let mut link_response = None;
                        let mut paint_color = color32(*color);
                        if let Some(href) = href {
                            let link_rect = Rect::from_min_size(
                                text_position,
                                vec2(
                                    wisp_engine::text_width_weighted(
                                        content,
                                        *font_size,
                                        *font_weight >= 600,
                                    )
                                    .max(1.0),
                                    wisp_engine::text_line_height(*font_size),
                                ),
                            );
                            let response = ui.interact(
                                link_rect,
                                ui.scope_id().with((
                                    "wisp_link",
                                    tab_id,
                                    x.to_bits(),
                                    y.to_bits(),
                                    content,
                                )),
                                Sense::click(),
                            );
                            if response.hovered() {
                                ui.output_mut(|output| {
                                    output.cursor_icon = egui::CursorIcon::PointingHand;
                                });
                                paint_color = p.accent;
                            }
                            if response.is_pointer_button_down_on() {
                                paint_color = p.accent;
                            }
                            link_response = Some((response, href));
                        }
                        painter.text(
                            text_position,
                            Align2::LEFT_TOP,
                            content,
                            font_id,
                            paint_color,
                        );
                        if let Some((response, href)) = link_response {
                            if response.is_pointer_button_down_on() {
                                let link_width = wisp_engine::text_width_weighted(
                                    content,
                                    *font_size,
                                    *font_weight >= 600,
                                );
                                let underline_y = text_position.y + *font_size * 0.82;
                                painter.line_segment(
                                    [
                                        pos2(text_position.x, underline_y),
                                        pos2(text_position.x + link_width, underline_y),
                                    ],
                                    Stroke::new(1.0, paint_color),
                                );
                            }
                            if response.clicked()
                                && let Ok(base) = url::Url::parse(&active_url)
                                && let Ok(target) = base.join(href)
                                && matches!(target.scheme(), "http" | "https")
                            {
                                navigate_to = Some(target.to_string());
                            }
                        }
                    }
                    wisp_engine::PaintCommand::Image { rect, url, href } => {
                        let image_rect = Rect::from_min_size(
                            page_rect.min + vec2(rect.x, rect.y),
                            vec2(rect.width, rect.height),
                        );
                        if let Some(texture) = wisp_image_texture(ui.ctx(), app, url) {
                            painter.image(
                                texture.id(),
                                image_rect,
                                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                                Color32::WHITE,
                            );
                        } else {
                            painter.rect_filled(image_rect, 0.0, p.surface_hi);
                            painter.rect_stroke(
                                image_rect,
                                0.0,
                                Stroke::new(1.0, p.border),
                                egui::StrokeKind::Inside,
                            );
                        }
                        if let Some(href) = href {
                            let response = ui.interact(
                                image_rect,
                                ui.scope_id().with((
                                    "wisp_image_link",
                                    tab_id,
                                    url,
                                    rect.x.to_bits(),
                                    rect.y.to_bits(),
                                )),
                                Sense::click(),
                            );
                            if response.hovered() {
                                ui.output_mut(|output| {
                                    output.cursor_icon = egui::CursorIcon::PointingHand;
                                });
                            }
                            if response.clicked()
                                && let Ok(base) = url::Url::parse(&active_url)
                                && let Ok(target) = base.join(href)
                                && matches!(target.scheme(), "http" | "https")
                            {
                                navigate_to = Some(target.to_string());
                            }
                        }
                    }
                    wisp_engine::PaintCommand::BackgroundImage { rect, image, .. } => {
                        let image_rect = Rect::from_min_size(
                            page_rect.min + vec2(rect.x, rect.y),
                            vec2(rect.width, rect.height),
                        );
                        match image {
                            wisp_engine::BgImage::Url(url) => {
                                if let Some(texture) = wisp_image_texture(ui.ctx(), app, url) {
                                    painter.image(
                                        texture.id(),
                                        image_rect,
                                        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                                        Color32::WHITE,
                                    );
                                }
                            }
                            wisp_engine::BgImage::LinearGradient { angle_deg, stops } => {
                                painter.add(egui::Shape::mesh(linear_gradient_mesh(
                                    image_rect, *angle_deg, stops,
                                )));
                            }
                        }
                    }
                }
            }
        });
    if let Some(url) = navigate_to {
        app.navigate_tab_by_id(tab_id, &url);
    }
    if open_in_webview {
        app.set_tab_engine(tab_id, BrowserEngine::WebView2);
    }
}

/// Extensión vertical de un comando en coordenadas del documento, para el
/// viewport culling del scroll de Wisp.
fn command_vspan(command: &wisp_engine::PaintCommand) -> (f32, f32) {
    match command {
        wisp_engine::PaintCommand::Text { y, font_size, .. } => {
            (*y, *y + wisp_engine::text_line_height(*font_size))
        }
        wisp_engine::PaintCommand::FillRect { rect, .. }
        | wisp_engine::PaintCommand::StrokeRect { rect, .. }
        | wisp_engine::PaintCommand::Image { rect, .. }
        | wisp_engine::PaintCommand::BackgroundImage { rect, .. } => (rect.y, rect.y + rect.height),
    }
}

fn warnings_need_webview(warnings: &[String]) -> bool {
    warnings.iter().any(|warning| {
        warning.contains("scripts externos")
            || warning.contains("script externo")
            || warning.contains("módulos ES")
            || warning.contains("módulo ES")
            || warning.contains("módulo externo")
            || warning.contains("módulo inline")
            || warning.contains("importaciones ES")
    })
}

fn color32(color: wisp_engine::Color) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r, color.g, color.b, color.a)
}

// ---------------------------------------------------------------- chrome

/// Ejecuta `add` en un `Ui` hijo limitado exactamente a `rect`. Cada sección del
/// chrome queda así en su sitio exacto, sin poder pintar ni capturar el ratón
/// fuera de su zona (clave con ventanas estrechas o muchas pestañas).
fn zone<R>(ui: &mut Ui, rect: Rect, add: impl FnOnce(&mut Ui) -> R) -> R {
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    add(&mut child)
}

/// Fila superior: marca, pestañas y controles de ventana pegados al borde
/// derecho. Devuelve `true` si el usuario pidió cerrar la aplicación.
fn draw_top_row(
    ui: &mut Ui,
    app: &mut Browser,
    p: &Palette,
    row: Rect,
    ctx: &egui::Context,
) -> bool {
    let brand_show_text = row.width() >= NARROW_CHROME;
    let brand_w = if brand_show_text {
        BRAND_WIDTH
    } else {
        BRAND_COMPACT_WIDTH
    };
    // Los controles de ventana son lo primero que se reserva: así el botón de
    // cerrar queda siempre contra el borde derecho, en cualquier pantalla.
    let controls_w = 3.0 * WINDOW_CONTROL_WIDTH;
    let tabs_w = (row.width() - brand_w - controls_w).max(0.0);

    let brand_rect = Rect::from_min_size(row.min, vec2(brand_w, row.height()));
    let tabs_rect = Rect::from_min_size(
        pos2(brand_rect.right(), row.top()),
        vec2(tabs_w, row.height()),
    );
    let controls_rect = Rect::from_min_max(pos2(row.right() - controls_w, row.top()), row.max);

    zone(ui, brand_rect, |ui| draw_brand(ui, app, p, brand_show_text));
    zone(ui, tabs_rect, |ui| draw_tab_strip(ui, app, p));
    draw_window_controls(ui, controls_rect, ctx, p, app.maximized)
}

/// Fila inferior: botones de navegación, barra de direcciones y acciones.
fn draw_nav_row(ui: &mut Ui, app: &mut Browser, p: &Palette, row: Rect) {
    let nav_w = 4.0 * TOOL_BUTTON + 3.0 * TOOL_SPACING;
    let show_all = row.width() >= NARROW_ACTIONS;
    // El botón de motor siempre está; el de lectura solo cuando la pestaña
    // Wisp activa ya tiene página maquetada.
    let reader_extra = if app.active_can_read() { 1.0 } else { 0.0 };
    let actions_w = if show_all {
        (5.0 + reader_extra) * TOOL_BUTTON + (4.0 + reader_extra) * TOOL_SPACING
    } else {
        (3.0 + reader_extra) * TOOL_BUTTON + (2.0 + reader_extra) * TOOL_SPACING
    };
    let nav_rect = Rect::from_min_size(row.min, vec2(nav_w, row.height()));
    let actions_rect = Rect::from_min_max(pos2(row.right() - actions_w, row.top()), row.max);
    // La barra de direcciones se queda con todo el hueco central.
    let addr_left = nav_rect.right() + SECTION_GAP;
    let addr_right = actions_rect.left() - SECTION_GAP;

    zone(ui, nav_rect, |ui| draw_toolbar_nav(ui, app, p));
    if addr_right > addr_left {
        let addr_rect =
            Rect::from_min_max(pos2(addr_left, row.top()), pos2(addr_right, row.bottom()));
        zone(ui, addr_rect, |ui| draw_address_bar(ui, app, p));
    }
    zone(ui, actions_rect, |ui| {
        draw_toolbar_actions(ui, app, p, show_all)
    });
}

fn apply_theme(ctx: &egui::Context, p: &Palette) {
    let mut visuals = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.panel_fill = p.chrome;
    visuals.window_fill = p.surface;
    visuals.extreme_bg_color = p.bg;
    visuals.faint_bg_color = p.surface;
    visuals.override_text_color = Some(p.text);
    // Selección translúcida para que el texto siga legible sobre el resaltado.
    visuals.selection.bg_fill = if p.dark {
        Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 0x30)
    } else {
        Color32::from_rgba_unmultiplied(0x00, 0x00, 0x00, 0x24)
    };
    visuals.selection.stroke = Stroke::new(1.0, p.text);
    visuals.hyperlink_color = p.accent;
    visuals.window_stroke = Stroke::new(1.0, p.border);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    visuals.widgets.noninteractive.bg_fill = p.chrome;
    visuals.widgets.noninteractive.weak_bg_fill = p.chrome;
    // Barra de título de los paneles (Ajustes, Marcadores, Historial).
    visuals.widgets.open.bg_fill = p.chrome;
    visuals.widgets.open.weak_bg_fill = p.chrome;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0, p.border);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, p.text);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, p.border);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
    visuals.widgets.inactive.bg_fill = p.surface;
    visuals.widgets.inactive.weak_bg_fill = p.surface;
    visuals.widgets.hovered.bg_fill = p.hover;
    visuals.widgets.hovered.weak_bg_fill = p.hover;
    visuals.widgets.active.bg_fill = p.surface_hi;
    visuals.widgets.active.weak_bg_fill = p.surface_hi;
    ctx.set_visuals(visuals);
}

// ---------------------------------------------------------------- iconos

const ICON_STROKE: f32 = 1.9;

fn spinner(painter: &egui::Painter, center: egui::Pos2, radius: f32, color: Color32, phase: f32) {
    let start = phase * std::f32::consts::TAU;
    let mut pts = Vec::with_capacity(26);
    for i in 0..=24 {
        let a = start + 4.2 * (i as f32 / 24.0);
        pts.push(pos2(
            center.x + radius * a.cos(),
            center.y + radius * a.sin(),
        ));
    }
    painter.add(egui::Shape::line(pts, Stroke::new(ICON_STROKE, color)));
}

fn chevron(painter: &egui::Painter, rect: Rect, dir: f32, color: Color32) {
    let c = rect.center();
    let tip = pos2(c.x + dir * 6.0, c.y);
    let joint = pos2(c.x + dir, c.y);
    let tail = pos2(c.x - dir * 6.0, c.y);
    let stroke = Stroke::new(ICON_STROKE, color);
    painter.line_segment([tail, joint], stroke);
    painter.line_segment([tip, pos2(joint.x, c.y - 5.0)], stroke);
    painter.line_segment([tip, pos2(joint.x, c.y + 5.0)], stroke);
}

fn reload_glyph(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let radius = 6.4;
    let start = 0.55_f32;
    let sweep = 4.7_f32;
    let mut pts = Vec::with_capacity(25);
    for i in 0..=24 {
        let a = start + sweep * (i as f32 / 24.0);
        pts.push(pos2(c.x + radius * a.cos(), c.y + radius * a.sin()));
    }
    painter.add(egui::Shape::line(pts, Stroke::new(ICON_STROKE, color)));

    let end = start + sweep;
    let tip = pos2(c.x + radius * end.cos(), c.y + radius * end.sin());
    let tangent = vec2(-end.sin(), end.cos());
    let normal = vec2(-tangent.y, tangent.x);
    let base = tip - tangent * 4.0;
    let stroke = Stroke::new(ICON_STROKE, color);
    painter.line_segment([base + normal * 2.7, tip], stroke);
    painter.line_segment([base - normal * 2.7, tip], stroke);
}

fn home_glyph(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let s = Stroke::new(ICON_STROKE, color);
    painter.add(egui::Shape::line(
        vec![
            pos2(c.x - 7.0, c.y - 1.0),
            pos2(c.x, c.y - 7.0),
            pos2(c.x + 7.0, c.y - 1.0),
        ],
        s,
    ));
    painter.add(egui::Shape::line(
        vec![
            pos2(c.x - 5.0, c.y - 2.0),
            pos2(c.x - 5.0, c.y + 6.0),
            pos2(c.x + 5.0, c.y + 6.0),
            pos2(c.x + 5.0, c.y - 2.0),
        ],
        s,
    ));
    painter.add(egui::Shape::line(
        vec![
            pos2(c.x - 1.5, c.y + 6.0),
            pos2(c.x - 1.5, c.y + 1.5),
            pos2(c.x + 1.5, c.y + 1.5),
            pos2(c.x + 1.5, c.y + 6.0),
        ],
        s,
    ));
}

fn globe(painter: &egui::Painter, center: egui::Pos2, r: f32, color: Color32) {
    let s = Stroke::new(1.7, color);
    painter.circle_stroke(center, r, s);
    let mut meridian = Vec::with_capacity(18);
    for i in 0..=16 {
        let a = i as f32 / 16.0 * std::f32::consts::TAU;
        meridian.push(pos2(center.x + r * 0.48 * a.sin(), center.y + r * a.cos()));
    }
    painter.add(egui::Shape::line(meridian, s));
    painter.line_segment(
        [pos2(center.x - r, center.y), pos2(center.x + r, center.y)],
        s,
    );
}

fn sliders_glyph(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let rows = [(-4.5_f32, -2.0_f32), (0.0, 3.0), (4.5, -3.5)];
    let stroke = Stroke::new(ICON_STROKE, color);
    for (y_offset, knob_x) in rows {
        let y = c.y + y_offset;
        let knob = pos2(c.x + knob_x, y);
        painter.line_segment([pos2(c.x - 7.0, y), pos2(knob.x - 2.0, y)], stroke);
        painter.line_segment([pos2(knob.x + 2.0, y), pos2(c.x + 7.0, y)], stroke);
        painter.circle_filled(knob, 2.0, color);
    }
}

fn history_glyph(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(ICON_STROKE, color);
    painter.circle_stroke(c, 6.5, stroke);
    painter.line_segment([c, pos2(c.x, c.y - 3.7)], stroke);
    painter.line_segment([c, pos2(c.x + 3.0, c.y + 1.5)], stroke);
}

fn bookmark_glyph(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    painter.add(egui::Shape::line(
        vec![
            pos2(c.x - 5.0, c.y - 6.5),
            pos2(c.x + 5.0, c.y - 6.5),
            pos2(c.x + 5.0, c.y + 6.5),
            pos2(c.x, c.y + 3.0),
            pos2(c.x - 5.0, c.y + 6.5),
            pos2(c.x - 5.0, c.y - 6.5),
        ],
        Stroke::new(ICON_STROKE, color),
    ));
}

fn star_glyph(painter: &egui::Painter, rect: Rect, color: Color32, filled: bool) {
    let c = rect.center();
    let mut points = Vec::with_capacity(10);
    for i in 0..10 {
        let angle = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
        let radius = if i % 2 == 0 { 6.8 } else { 3.0 };
        points.push(pos2(c.x + radius * angle.cos(), c.y + radius * angle.sin()));
    }
    if filled {
        for i in 0..points.len() {
            painter.add(egui::Shape::convex_polygon(
                vec![c, points[i], points[(i + 1) % points.len()]],
                color,
                Stroke::NONE,
            ));
        }
    }
    let mut outline = points;
    let first = outline[0];
    outline.push(first);
    painter.add(egui::Shape::line(outline, Stroke::new(ICON_STROKE, color)));
}

/// Selector de motor: un dial con aguja — el punto central se rellena cuando
/// la pestaña usa Wisp (el motor ligero de Ghost).
fn engine_glyph(painter: &egui::Painter, rect: Rect, color: Color32, wisp: bool) {
    let c = rect.center();
    let stroke = Stroke::new(ICON_STROKE, color);
    painter.circle_stroke(c, 6.6, stroke);
    // Aguja apuntando a "Wisp" (arriba) o a "WebView2" (abajo-derecha).
    let angle = if wisp {
        -std::f32::consts::FRAC_PI_2
    } else {
        std::f32::consts::FRAC_PI_4
    };
    let tip = pos2(c.x + 4.6 * angle.cos(), c.y + 4.6 * angle.sin());
    painter.line_segment([c, tip], stroke);
    if wisp {
        painter.circle_filled(c, 1.6, color);
    }
}

/// Modo lectura: un libro abierto — dos páginas y el lomo.
fn reader_glyph(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(ICON_STROKE, color);
    painter.add(egui::Shape::line(
        vec![
            pos2(c.x - 6.8, c.y - 5.4),
            pos2(c.x - 6.8, c.y + 5.4),
            pos2(c.x - 0.6, c.y + 3.2),
            pos2(c.x - 0.6, c.y - 3.4),
            pos2(c.x - 6.8, c.y - 5.4),
        ],
        stroke,
    ));
    painter.add(egui::Shape::line(
        vec![
            pos2(c.x + 6.8, c.y - 5.4),
            pos2(c.x + 6.8, c.y + 5.4),
            pos2(c.x + 0.6, c.y + 3.2),
            pos2(c.x + 0.6, c.y - 3.4),
            pos2(c.x + 6.8, c.y - 5.4),
        ],
        stroke,
    ));
    painter.line_segment(
        [pos2(c.x - 0.6, c.y - 4.6), pos2(c.x + 0.6, c.y - 4.6)],
        stroke,
    );
}

/// Devuelve la textura egui de una imagen Wisp, decodificándola una sola vez
/// por URL y guardándola en la caché de la pestaña activa.
fn wisp_image_texture(
    ctx: &egui::Context,
    app: &mut Browser,
    url: &str,
) -> Option<egui::TextureHandle> {
    if let Some(texture) = app.wisp_textures.get(url) {
        // Mover al final del orden (LRU: más reciente)
        if let Some(pos) = app.wisp_texture_order.iter().position(|x| x == url) {
            app.wisp_texture_order.remove(pos);
        }
        app.wisp_texture_order.push_back(url.to_string());
        return Some(texture.clone());
    }
    let bytes = app
        .active_tab()?
        .wisp_html
        .as_ref()?
        .images
        .get(url)?
        .data
        .clone();
    let decoded = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let size = [decoded.width() as usize, decoded.height() as usize];
    let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &decoded.into_raw());
    let texture = ctx.load_texture(url, color_image, egui::TextureOptions::LINEAR);

    // LRU suave: eliminar solo la más antigua si excede el límite
    if app.wisp_textures.len() >= crate::browser::MAX_WISP_TEXTURES {
        if let Some(old_url) = app.wisp_texture_order.pop_front() {
            app.wisp_textures.remove(&old_url);
        }
    }
    app.wisp_texture_order.push_back(url.to_owned());
    app.wisp_textures.insert(url.to_owned(), texture.clone());
    Some(texture)
}

/// Degradado lineal CSS como `egui::Mesh` de 4 vértices: el color de cada
/// esquina se calcula proyectando su posición sobre el eje del degradado,
/// replicando la "gradient line" de la especificación (0deg = arriba,
/// 90deg = derecha, sentido horario).
fn linear_gradient_mesh(
    rect: Rect,
    angle_deg: f32,
    stops: &[wisp_engine::GradientStop],
) -> egui::epaint::Mesh {
    let radians = angle_deg.to_radians();
    let direction = vec2(radians.sin(), -radians.cos());
    let center = rect.center();
    let corners = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
    ];
    let projections: Vec<f32> = corners
        .iter()
        .map(|corner| (*corner - center).dot(direction))
        .collect();
    let min = projections.iter().copied().fold(f32::INFINITY, f32::min);
    let max = projections
        .iter()
        .copied()
        .fold(f32::NEG_INFINITY, f32::max);
    let span = (max - min).max(f32::EPSILON);
    let mut mesh = egui::epaint::Mesh::default();
    for (corner, projection) in corners.iter().zip(projections) {
        let t = ((projection - min) / span).clamp(0.0, 1.0);
        mesh.vertices.push(egui::epaint::Vertex {
            pos: *corner,
            uv: pos2(0.0, 0.0),
            color: gradient_color_at(stops, t),
        });
    }
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    mesh
}

/// Color del degradado en la posición `t` (0–1), interpolando entre las dos
/// paradas que la rodean. Las paradas se ordenan por offset por si el autor
/// las declara desordenadas.
fn gradient_color_at(stops: &[wisp_engine::GradientStop], t: f32) -> Color32 {
    let mut sorted: Vec<&wisp_engine::GradientStop> = stops.iter().collect();
    sorted.sort_by(|a, b| a.offset.total_cmp(&b.offset));
    let Some(first) = sorted.first() else {
        return Color32::TRANSPARENT;
    };
    if t <= first.offset {
        return color32(first.color);
    }
    for pair in sorted.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if t <= b.offset {
            let span = (b.offset - a.offset).max(f32::EPSILON);
            let k = ((t - a.offset) / span).clamp(0.0, 1.0);
            let lerp = |from: u8, to: u8| (from as f32 + (to as f32 - from as f32) * k) as u8;
            return color32(wisp_engine::Color {
                r: lerp(a.color.r, b.color.r),
                g: lerp(a.color.g, b.color.g),
                b: lerp(a.color.b, b.color.b),
                a: lerp(a.color.a, b.color.a),
            });
        }
    }
    sorted
        .last()
        .map(|stop| color32(stop.color))
        .unwrap_or(Color32::TRANSPARENT)
}

fn logo_texture(ctx: &egui::Context, app: &mut Browser) -> Option<egui::TextureHandle> {
    if app.logo_tex.is_none() {
        let img = crate::logo::icon(64);
        let size = [img.width() as usize, img.height() as usize];
        let image = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
        app.logo_tex = Some(ctx.load_texture("ghost_logo", image, egui::TextureOptions::LINEAR));
    }
    app.logo_tex.clone()
}

fn background_texture(ctx: &egui::Context, app: &mut Browser) -> Option<egui::TextureHandle> {
    // Usar el mismo identificador que se usa en wisp_textures para la imagen de fondo
    const BG_URL: &str = "ghost://black.webp";
    if let Some(tex) = app.wisp_textures.get(BG_URL) {
        return Some(tex.clone());
    }
    // Cargar la imagen de fondo incrustada
    let bytes = include_bytes!("../black.webp");
    let decoded = image::load_from_memory(bytes).ok()?.to_rgba8();
    let size = [decoded.width() as usize, decoded.height() as usize];
    let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &decoded.into_raw());
    let texture = ctx.load_texture(BG_URL, color_image, egui::TextureOptions::LINEAR);
    app.wisp_textures.insert(BG_URL.to_string(), texture.clone());
    Some(texture)
}

fn paint_logo(ui: &mut Ui, rect: Rect, app: &mut Browser) {
    if let Some(tex) = logo_texture(ui.ctx(), app) {
        let target = Rect::from_center_size(rect.center(), rect.size());
        // La marca es blanca; en tema claro hay que teñirla de oscuro para
        // que no desaparezca sobre el chrome blanco.
        let tint = ui.visuals().override_text_color.unwrap_or(Color32::WHITE);
        ui.painter().image(
            tex.id(),
            target,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            tint,
        );
    }
}

// ---------------------------------------------------------------- pestañas

fn galley(
    ui: &Ui,
    text: &str,
    color: Color32,
    max_width: f32,
    size: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::default();
    job.wrap = egui::text::TextWrapping {
        max_width,
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
        ..Default::default()
    };
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: FontId::proportional(size),
            color,
            ..Default::default()
        },
    );
    ui.painter().layout_job(job)
}

fn draw_brand(ui: &mut Ui, app: &mut Browser, p: &Palette, show_text: bool) {
    let rect = ui.available_rect_before_wrap();
    let response = ui.interact(rect, ui.scope_id().with("brand"), Sense::click_and_drag());
    if response.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    if response.double_clicked() {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Maximized(!app.maximized));
    }

    let logo_size = 20.0;
    let logo_rect = Rect::from_center_size(
        pos2(rect.left() + 14.0, rect.center().y),
        vec2(logo_size, logo_size),
    );
    paint_logo(ui, logo_rect, app);
    if show_text {
        ui.painter().text(
            pos2(rect.left() + 29.0, rect.center().y),
            Align2::LEFT_CENTER,
            "Ghost",
            FontId::proportional(14.0),
            p.text,
        );
    }
}

fn draw_window_controls(
    ui: &mut Ui,
    rect: Rect,
    ctx: &egui::Context,
    p: &Palette,
    maximized: bool,
) -> bool {
    #[derive(Clone, Copy)]
    enum Action {
        Minimize,
        ToggleMaximize,
        Close,
    }

    let mut close_requested = false;
    let actions = [Action::Minimize, Action::ToggleMaximize, Action::Close];
    let count = actions.len() as f32;
    // Se colocan desde la derecha: el último control acaba pegado al borde.
    for (index, action) in actions.into_iter().enumerate() {
        let left = rect.right() - WINDOW_CONTROL_WIDTH * (count - index as f32);
        let btn = Rect::from_min_max(
            pos2(left, rect.top()),
            pos2(left + WINDOW_CONTROL_WIDTH, rect.bottom()),
        );
        let response = ui.interact(
            btn,
            egui::Id::unique(("ghost_window_control", index)),
            Sense::click(),
        );
        let closing = matches!(action, Action::Close);
        if response.hovered() {
            let fill = if closing {
                Color32::from_rgb(0xC4, 0x2B, 0x1C)
            } else {
                p.tab_hover
            };
            ui.painter().rect_filled(btn, 0.0, fill);
        }

        let color = if closing && response.hovered() {
            Color32::WHITE
        } else {
            p.muted_strong
        };
        let center = btn.center();
        let stroke = Stroke::new(1.4, color);
        match action {
            Action::Minimize => {
                ui.painter().line_segment(
                    [
                        pos2(center.x - 5.0, center.y),
                        pos2(center.x + 5.0, center.y),
                    ],
                    stroke,
                );
            }
            Action::ToggleMaximize if maximized => {
                let back =
                    Rect::from_center_size(pos2(center.x + 1.5, center.y - 1.5), vec2(9.0, 8.0));
                let front =
                    Rect::from_center_size(pos2(center.x - 1.5, center.y + 1.5), vec2(9.0, 8.0));
                for outline in [back, front] {
                    ui.painter().rect_stroke(
                        outline,
                        egui::CornerRadius::same(1),
                        stroke,
                        egui::StrokeKind::Inside,
                    );
                }
            }
            Action::ToggleMaximize => {
                let outline = Rect::from_center_size(center, vec2(11.0, 10.0));
                ui.painter().rect_stroke(
                    outline,
                    egui::CornerRadius::same(1),
                    stroke,
                    egui::StrokeKind::Inside,
                );
            }
            Action::Close => {
                ui.painter().line_segment(
                    [
                        pos2(center.x - 5.0, center.y - 5.0),
                        pos2(center.x + 5.0, center.y + 5.0),
                    ],
                    stroke,
                );
                ui.painter().line_segment(
                    [
                        pos2(center.x + 5.0, center.y - 5.0),
                        pos2(center.x - 5.0, center.y + 5.0),
                    ],
                    stroke,
                );
            }
        }

        if response.clicked() {
            match action {
                Action::Minimize => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
                Action::ToggleMaximize => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }
                Action::Close => close_requested = true,
            }
        }
    }
    close_requested
}

enum TabAction {
    Close(usize),
    Switch(usize),
}

fn draw_tab_strip(ui: &mut Ui, app: &mut Browser, p: &Palette) {
    let plus_w = NEW_TAB_WIDTH;
    let area_w = (ui.available_width() - plus_w - SECTION_GAP).max(0.0);
    let count = app.tabs.len();
    let spacing = 3.0;
    // Las pestañas se reparten el ancho disponible; si son muchas encogen hasta
    // el mínimo y el resto queda recortado por el `section` que las contiene.
    let width = if count == 0 {
        36.0
    } else {
        ((area_w - spacing * (count as f32 - 1.0)) / count as f32).clamp(36.0, 220.0)
    };
    let used_w = if count == 0 {
        0.0
    } else {
        width * count as f32 + spacing * (count as f32 - 1.0)
    };
    let overflow = used_w > area_w + 0.5;
    // Al cambiar de pestaña hay que traerla a la vista si la tira se desplaza.
    let active_changed = ui.data_mut(|d| {
        let id = egui::Id::unique("ghost_tabs_active_seen");
        let changed = d.get_temp::<usize>(id) != Some(app.active);
        d.insert_temp(id, app.active);
        changed
    });

    let mut tab_action = None;
    if overflow {
        // Con muchas pestañas, la tira se desplaza en horizontal y el botón «+»
        // queda fijo a la derecha para que nunca desaparezca.
        let scroll_w = area_w;
        ui.allocate_ui(vec2(scroll_w, 30.0), |ui| {
            egui::ScrollArea::horizontal()
                .id_salt("ghost_tabs_scroll")
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = spacing;
                    ui.horizontal(|ui| {
                        for i in 0..count {
                            if let Some(action) = draw_tab(ui, width, app, p, i, active_changed) {
                                if tab_action.is_none() {
                                    tab_action = Some(action);
                                }
                            }
                        }
                    });
                });
        });
    } else {
        ui.spacing_mut().item_spacing.x = spacing;
        for i in 0..count {
            if let Some(action) = draw_tab(ui, width, app, p, i, false) {
                if tab_action.is_none() {
                    tab_action = Some(action);
                }
            }
        }
    }
    if let Some(action) = tab_action {
        match action {
            TabAction::Close(idx) => app.close_tab(idx),
            TabAction::Switch(idx) => app.switch_tab(idx),
        }
    }

    // Botón «+» de nueva pestaña, justo después de la última pestaña.
    ui.spacing_mut().item_spacing.x = SECTION_GAP;
    let (rect, _) = ui.allocate_exact_size(vec2(plus_w, 26.0), Sense::hover());
    let resp = ui.interact(rect, egui::Id::unique("ghost_new_tab"), Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(7), p.tab_hover);
    }
    let c = rect.center();
    let col = if resp.hovered() {
        p.text
    } else {
        p.muted_strong
    };
    ui.painter().line_segment(
        [pos2(c.x - 4.5, c.y), pos2(c.x + 4.5, c.y)],
        Stroke::new(ICON_STROKE, col),
    );
    ui.painter().line_segment(
        [pos2(c.x, c.y - 4.5), pos2(c.x, c.y + 4.5)],
        Stroke::new(ICON_STROKE, col),
    );
    if resp.on_hover_text("Nueva pestaña (Ctrl+T)").clicked() {
        app.request_new_tab();
    }

    // El espacio que sobra sirve para arrastrar o maximizar la ventana.
    ui.spacing_mut().item_spacing.x = 0.0;
    let drag_width = ui.available_width().max(0.0);
    let (_, drag_resp) = ui.allocate_exact_size(vec2(drag_width, 26.0), Sense::click_and_drag());
    if drag_resp.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    if drag_resp.double_clicked() {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Maximized(!app.maximized));
    }
}

fn draw_tab(
    ui: &mut Ui,
    width: f32,
    app: &mut Browser,
    p: &Palette,
    index: usize,
    reveal: bool,
) -> Option<TabAction> {
    let Some(t) = app.tabs.get(index) else {
        return None;
    };
    let is_active = index == app.active;
    // La nueva pestaña nativa no está "suspendida": se ve como cualquier
    // pestaña activa aunque no tenga proceso.
    let live = t.is_live() || t.is_newtab();
    let (id, title, loading) = (t.id, t.display_title(), t.loading);

    let max_chars = (((width - 44.0) / 6.5).max(2.0)) as usize;
    let mut short: String = title.chars().take(max_chars).collect();
    if title.chars().count() > max_chars {
        short.push('…');
    }
    // Las suspendidas se ven más apagadas: pista visual del ahorro de memoria.
    let color = if is_active {
        p.text
    } else if live {
        p.muted_strong
    } else {
        p.muted
    };

    let resp = ui.add(
        egui::Button::new(RichText::new(short).size(12.0).color(color))
            .fill(if is_active {
                p.tab_active
            } else {
                Color32::TRANSPARENT
            })
            .stroke(Stroke::NONE)
            .corner_radius(egui::CornerRadius::same(8))
            .min_size(vec2(width, 30.0))
            .truncate(),
    );
    if reveal && is_active {
        ui.scroll_to_rect(resp.rect, Some(egui::Align::Center));
    }

    if loading && width >= 80.0 {
        let phase = (ui.input(|i| i.time) as f32 * 0.8).fract();
        spinner(
            ui.painter(),
            pos2(resp.rect.left() + 10.0, resp.rect.center().y),
            3.0,
            p.text,
            phase,
        );
        ui.ctx().request_repaint();
    }

    if is_active {
        let bar = Rect::from_center_size(
            pos2(resp.rect.center().x, resp.rect.bottom() - 1.0),
            vec2((width * 0.45).min(44.0), 2.0),
        );
        ui.painter()
            .rect_filled(bar, egui::CornerRadius::same(1), p.accent);
    }

    // Cerrar dentro de la pestaña (en pestañas muy estrechas solo al pasar el ratón)
    let close_rect = Rect::from_center_size(
        pos2(resp.rect.right() - 12.0, resp.rect.center().y),
        vec2(16.0, 16.0),
    );
    let close_resp = ui.interact(
        close_rect,
        egui::Id::unique(("tab_close", id)),
        Sense::click(),
    );
    let show_close = (width >= 65.0 && is_active) || resp.hovered() || close_resp.hovered();
    if show_close {
        if close_resp.hovered() {
            ui.painter()
                .rect_filled(close_rect, egui::CornerRadius::same(6), p.danger);
        }
        let c = close_rect.center();
        let r = 3.6;
        let col = if close_resp.hovered() {
            p.bg
        } else {
            p.muted_strong
        };
        let st = Stroke::new(ICON_STROKE, col);
        ui.painter()
            .line_segment([pos2(c.x - r, c.y - r), pos2(c.x + r, c.y + r)], st);
        ui.painter()
            .line_segment([pos2(c.x + r, c.y - r), pos2(c.x - r, c.y + r)], st);
    }

    if close_resp.clicked() {
        return Some(TabAction::Close(index));
    }
    if resp.middle_clicked() {
        return Some(TabAction::Close(index));
    }
    if resp.clicked() {
        return Some(TabAction::Switch(index));
    }
    let tip = if live {
        title
    } else {
        format!(
            "{title}
Suspendida para ahorrar memoria"
        )
    };
    resp.on_hover_text(tip);
    None
}

// ---------------------------------------------------------------- barra

fn icon_button(
    ui: &mut Ui,
    p: &Palette,
    enabled: bool,
    tip: &str,
    paint: impl FnOnce(&egui::Painter, Rect, Color32),
) -> bool {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(TOOL_BUTTON, TOOL_BUTTON), sense);
    if enabled && resp.hovered() {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(9), p.hover);
    }
    let color = if enabled { p.text } else { p.muted };
    paint(ui.painter(), rect, color);
    if enabled {
        resp.on_hover_text(tip).clicked()
    } else {
        resp.on_hover_text(tip);
        false
    }
}

fn draw_toolbar_nav(ui: &mut Ui, app: &mut Browser, p: &Palette) {
    ui.spacing_mut().item_spacing.x = TOOL_SPACING;

    let back = app.can_go_back();
    if icon_button(ui, p, back, "Atrás (Alt+←)", |pt, r, c| {
        chevron(pt, r, -1.0, c)
    }) {
        app.go_back();
    }
    let fwd = app.can_go_forward();
    if icon_button(ui, p, fwd, "Adelante (Alt+→)", |pt, r, c| {
        chevron(pt, r, 1.0, c)
    }) {
        app.go_forward();
    }
    if icon_button(ui, p, true, "Recargar (Ctrl+R)", |pt, r, c| {
        reload_glyph(pt, r, c)
    }) {
        app.reload();
    }
    if icon_button(ui, p, true, "Página de inicio", |pt, r, c| {
        home_glyph(pt, r, c)
    }) {
        app.go_home();
    }
}

fn glyph_button(
    ui: &mut Ui,
    p: &Palette,
    tip: &str,
    paint: impl FnOnce(&egui::Painter, Rect, Color32),
) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(TOOL_BUTTON, TOOL_BUTTON), Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(9), p.hover);
    }
    paint(
        ui.painter(),
        rect,
        if resp.hovered() {
            p.text
        } else {
            p.muted_strong
        },
    );
    resp.on_hover_text(tip).clicked()
}

fn draw_toolbar_actions(ui: &mut Ui, app: &mut Browser, p: &Palette, show_all: bool) {
    ui.spacing_mut().item_spacing.x = TOOL_SPACING;

    // Marcador de la página actual.
    let starred = app.active_is_bookmarked();
    let (rect, resp) = ui.allocate_exact_size(vec2(TOOL_BUTTON, TOOL_BUTTON), Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(9), p.hover);
    }
    let star_color = if starred {
        p.accent
    } else if resp.hovered() {
        p.text
    } else {
        p.muted_strong
    };
    star_glyph(ui.painter(), rect, star_color, starred);
    let tip = if starred {
        "Quitar de marcadores (Ctrl+D)"
    } else {
        "Guardar en marcadores (Ctrl+D)"
    };
    if resp.on_hover_text(tip).clicked() {
        app.toggle_bookmark_active();
    }

    // Motor de la pestaña: Wisp (liviano) ↔ WebView2 (compatible).
    let engine = app
        .active_tab()
        .map(|tab| tab.engine)
        .unwrap_or(BrowserEngine::WebView2);
    let has_url = app
        .active_tab()
        .is_some_and(|tab| !tab.display_url().is_empty());
    let wisp_active = engine == BrowserEngine::Wisp;
    let (rect, resp) = ui.allocate_exact_size(vec2(TOOL_BUTTON, TOOL_BUTTON), Sense::click());
    if resp.hovered() && has_url {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(9), p.hover);
    }
    let engine_color = if !has_url {
        p.muted
    } else if wisp_active {
        p.accent
    } else if resp.hovered() {
        p.text
    } else {
        p.muted_strong
    };
    engine_glyph(ui.painter(), rect, engine_color, wisp_active);
    let engine_tip = format!(
        "Motor: {} — clic para cambiar (Ctrl+Shift+W)",
        engine.label()
    );
    if has_url && resp.on_hover_text(engine_tip).clicked() {
        app.toggle_engine_active();
    }

    // Modo lectura: texto limpio del DOM de Wisp, listo para IA/lectura.
    if app.active_can_read() {
        if glyph_button(ui, p, "Modo lectura", |pt, r, c| reader_glyph(pt, r, c)) {
            app.panel = if app.panel == Panel::Reader {
                Panel::None
            } else {
                Panel::Reader
            };
        }
    }

    if show_all {
        if glyph_button(ui, p, "Marcadores (Ctrl+B)", |pt, r, c| {
            bookmark_glyph(pt, r, c)
        }) {
            app.panel = if app.panel == Panel::Bookmarks {
                Panel::None
            } else {
                Panel::Bookmarks
            };
        }
        if glyph_button(ui, p, "Historial (Ctrl+H)", |pt, r, c| {
            history_glyph(pt, r, c)
        }) {
            app.panel = if app.panel == Panel::History {
                Panel::None
            } else {
                Panel::History
            };
        }
    }

    if glyph_button(ui, p, "Ajustes", |pt, r, c| sliders_glyph(pt, r, c)) {
        app.panel = if app.panel == Panel::Settings {
            Panel::None
        } else {
            Panel::Settings
        };
    }
}

fn draw_address_bar(ui: &mut Ui, app: &mut Browser, p: &Palette) {
    let focused = app.addr_editing;
    let frame = egui::Frame::NONE
        .fill(if focused { p.surface_hi } else { p.surface })
        .stroke(Stroke::new(1.0, if focused { p.accent } else { p.border }))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin {
            left: 8,
            right: 8,
            top: 4,
            bottom: 4,
        });
    let mut submitted = false;

    let addr_id = egui::Id::unique("address_bar");
    // El "seleccionar todo" hay que fijarlo antes de dibujar el TextEdit: si se
    // hace después, el propio widget guarda su estado y lo pisa.
    if app.addr_select_all {
        if let Some(mut state) = egui::text_edit::TextEditState::load(ui.ctx(), addr_id) {
            let len = app.addr_text.chars().count();
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(len),
                )));
            state.store(ui.ctx(), addr_id);
        }
        app.addr_select_all = false;
    }

    // El marco ocupa el ancho de la sección y se centra en la fila; dentro, el
    // icono y el campo se colocan en un rectángulo de alto fijo para que queden
    // alineados con el resto de la barra a cualquier tamaño.
    frame.show(ui, |ui| {
        let row = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), ADDR_ROW_HEIGHT));
        let icon_rect = Rect::from_min_size(row.min, vec2(14.0, row.height()));
        globe(ui.painter(), icon_rect.center(), 5.5, p.muted_strong);
        let edit_rect = Rect::from_min_max(pos2(icon_rect.right() + 6.0, row.top()), row.max);
        let resp = ui.put(
            edit_rect,
            egui::TextEdit::singleline(&mut app.addr_text)
                .id(addr_id)
                .desired_width(edit_rect.width())
                .frame(egui::Frame::NONE)
                .hint_text(
                    RichText::new("Buscar en la web o escribir una dirección").color(p.muted),
                )
                .font(egui::TextStyle::Body),
        );
        app.addr_editing = resp.has_focus();
        // Al entrar en la barra (clic o Ctrl+L) se selecciona todo, como en
        // cualquier navegador: escribir reemplaza la URL actual.
        if resp.gained_focus() {
            app.addr_select_all = true;
        }
        if app.addr_focus_request {
            resp.request_focus();
            app.addr_focus_request = false;
            app.addr_select_all = true;
            // Si el foco del sistema estaba en la webview (Ctrl+L desde una
            // página), hay que devolverlo a la ventana o el teclado sigue
            // escribiendo en la página.
            app.focus_window_request = true;
        }
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            submitted = true;
        }
        if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            app.addr_text = app
                .active_tab()
                .map(|t| t.display_url())
                .unwrap_or_default();
            resp.surrender_focus();
        }
    });

    if submitted {
        let input = app.addr_text.clone();
        app.navigate_active(&input);
    }
}

// ---------------------------------------------------------------- paneles

fn panel_window<'a>(
    id: &'a str,
    title: &'a str,
    size: egui::Vec2,
    p: &Palette,
) -> egui::Window<'a> {
    egui::Window::new(title)
        .id(egui::Id::unique(id))
        .default_pos(pos2(180.0, 120.0))
        .default_size(size)
        .collapsible(false)
        .resizable(true)
        .frame(
            egui::Frame::window(&egui::Style::default())
                .fill(p.surface)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(egui::CornerRadius::same(14))
                .inner_margin(egui::Margin::same(14)),
        )
}

fn section(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add_space(6.0);
    ui.label(
        RichText::new(text.to_uppercase())
            .size(10.0)
            .strong()
            .color(p.muted),
    );
    ui.add_space(2.0);
}

fn list_row(ui: &mut Ui, p: &Palette, title: &str, sub: &str, show_delete: bool) -> (bool, bool) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
    let painter = ui.painter().clone();
    let hover = resp.hovered();
    if hover {
        painter.rect_filled(rect, egui::CornerRadius::same(8), p.hover);
    }
    let title_g = galley(ui, title, p.text, (rect.width() - 44.0).max(20.0), 12.5);
    painter.galley(pos2(rect.left() + 9.0, rect.top() + 4.0), title_g, p.text);
    if !sub.is_empty() {
        let sub_g = galley(ui, sub, p.muted, (rect.width() - 44.0).max(20.0), 10.5);
        painter.galley(pos2(rect.left() + 9.0, rect.top() + 19.0), sub_g, p.muted);
    }
    let mut deleted = false;
    if show_delete && hover {
        let x_rect =
            Rect::from_center_size(pos2(rect.right() - 14.0, rect.center().y), vec2(18.0, 18.0));
        let x_resp = ui.interact(
            x_rect,
            egui::Id::unique(("row_del", title, sub)),
            Sense::click(),
        );
        if x_resp.hovered() {
            ui.painter()
                .rect_filled(x_rect, egui::CornerRadius::same(6), p.danger);
        }
        let c = x_rect.center();
        let r = 3.4;
        let s = Stroke::new(
            ICON_STROKE,
            if x_resp.hovered() {
                p.bg
            } else {
                p.muted_strong
            },
        );
        ui.painter()
            .line_segment([pos2(c.x - r, c.y - r), pos2(c.x + r, c.y + r)], s);
        ui.painter()
            .line_segment([pos2(c.x + r, c.y - r), pos2(c.x - r, c.y + r)], s);
        deleted = x_resp.clicked();
    }
    (resp.clicked() && !deleted, deleted)
}

fn draw_panels(ctx: &egui::Context, app: &mut Browser, p: &Palette) {
    match app.panel {
        Panel::None => {}
        Panel::Bookmarks => {
            let mut navigate_to: Option<String> = None;
            let mut remove: Option<usize> = None;
            let mut add_current = false;
            let mut close = false;
            let entries: Vec<(String, String)> = app
                .store
                .bookmarks
                .iter()
                .map(|b| (b.title.clone(), b.url.clone()))
                .collect();

            panel_window("panel_bookmarks", "Marcadores", vec2(380.0, 440.0), p).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Añadir página actual").clicked() {
                        add_current = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cerrar").clicked() {
                            close = true;
                        }
                    });
                });
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if entries.is_empty() {
                            ui.label(
                                RichText::new(
                                    "Sin marcadores todavía. Guarda una página con el botón de estrella.",
                                )
                                    .size(12.0)
                                    .color(p.muted),
                            );
                        }
                        for (i, (title, url)) in entries.iter().enumerate() {
                            let (open, del) = list_row(ui, p, title, url, true);
                            if open {
                                navigate_to = Some(url.clone());
                            }
                            if del {
                                remove = Some(i);
                            }
                        }
                    });
            });

            if add_current {
                app.add_bookmark_current();
            }
            if close {
                app.panel = Panel::None;
            }
            if let Some(url) = navigate_to {
                app.navigate_active(&url);
                app.panel = Panel::None;
            }
            if let Some(i) = remove {
                app.store.remove_bookmark(i);
            }
        }
        Panel::History => {
            let mut navigate_to: Option<String> = None;
            let mut clear = false;
            let mut close = false;
            let entries: Vec<(String, String)> = app
                .store
                .recent_history(200)
                .into_iter()
                .map(|e| (e.title.clone(), e.url.clone()))
                .collect();

            panel_window("panel_history", "Historial", vec2(420.0, 460.0), p).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Limpiar historial").clicked() {
                        clear = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cerrar").clicked() {
                            close = true;
                        }
                    });
                });
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if entries.is_empty() {
                            ui.label(
                                RichText::new("El historial está vacío.")
                                    .size(12.0)
                                    .color(p.muted),
                            );
                        }
                        for (title, url) in &entries {
                            let label = if title.is_empty() { url } else { title };
                            let (open, _) = list_row(ui, p, label, url, false);
                            if open {
                                navigate_to = Some(url.clone());
                            }
                        }
                    });
            });

            if close {
                app.panel = Panel::None;
            }
            if let Some(url) = navigate_to {
                app.navigate_active(&url);
                app.panel = Panel::None;
            }
            if clear {
                app.store.clear_history();
            }
        }
        Panel::Reader => {
            let reader = app.active_reader_text();
            let title = app
                .active_tab()
                .map(|tab| tab.display_title())
                .unwrap_or_default();
            let mut close = false;
            let mut copied = false;

            panel_window("panel_reader", "Lectura", vec2(560.0, 520.0), p).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&title).size(13.0).strong().color(p.text));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Copiar").clicked() {
                            copied = true;
                        }
                        if ui.button("Cerrar").clicked() {
                            close = true;
                        }
                    });
                });
                ui.add_space(6.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match &reader {
                        Some(text) => {
                            ui.add(
                                egui::Label::new(RichText::new(text).size(13.0).color(p.text))
                                    .selectable(true)
                                    .wrap(),
                            );
                        }
                        None => {
                            ui.label(
                                RichText::new("Esta página no tiene texto legible.")
                                    .size(12.0)
                                    .color(p.muted),
                            );
                        }
                    });
            });

            if copied && let Some(text) = &reader {
                ctx.copy_text(text.clone());
            }
            if close {
                app.panel = Panel::None;
            }
        }
        Panel::Settings => {
            let mut s = app.store.settings.clone();
            let mut changed = false;
            let mut close = false;
            let mut clear_data = false;
            let active_tab_id = app.active_tab().map(|tab| tab.id);
            let mut active_engine = app
                .active_tab()
                .map(|tab| tab.engine)
                .unwrap_or(s.default_engine);
            let mut active_engine_changed = false;
            let live = app.live_count();
            let suspended = app.suspended_count();
            let reclaimed = app.suspended_this_session;
            let version = wry::webview_version().unwrap_or_else(|_| "?".into());

            panel_window("panel_settings", "Ajustes", vec2(460.0, 640.0), p).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::hover());
                    paint_logo(ui, r, app);
                    ui.label(RichText::new("Ghost").size(18.0).strong().color(p.text));
                });
                ui.label(
                    RichText::new("Ligero por dentro, hermoso por fuera.")
                        .size(11.5)
                        .color(p.muted),
                );

                section(ui, p, "Memoria");
                if ui
                    .checkbox(&mut s.memory_saver, "Suspender pestañas inactivas")
                    .changed()
                {
                    changed = true;
                }
                ui.label(
                    RichText::new(
                        "Las pestañas que dejas de usar liberan su proceso y se \
                         restauran al volver a ellas.",
                    )
                    .size(11.0)
                    .color(p.muted),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Suspender tras").size(12.0).color(p.text));
                    let mins = (s.suspend_after_secs / 60).max(1) as u32;
                    let mut mins_f = mins as f64;
                    if ui
                        .add(
                            egui::Slider::new(&mut mins_f, 1.0..=60.0)
                                .suffix(" min")
                                .fixed_decimals(0),
                        )
                        .changed()
                    {
                        s.suspend_after_secs = (mins_f.round() as u64) * 60;
                        changed = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Máximo de pestañas vivas")
                            .size(12.0)
                            .color(p.text),
                    );
                    let mut max_live = s.max_live_tabs.max(1) as u32;
                    if ui.add(egui::Slider::new(&mut max_live, 1..=24)).changed() {
                        s.max_live_tabs = max_live as usize;
                        changed = true;
                    }
                });
                ui.label(
                    RichText::new(format!(
                        "Ahora: {live} con proceso · {suspended} suspendidas · \
                         {reclaimed} liberadas en esta sesión"
                    ))
                    .size(11.0)
                    .color(p.accent),
                );

                section(ui, p, "Privacidad y Rendimiento");
                if ui
                    .checkbox(
                        &mut s.block_ads,
                        "Bloquear dominios conocidos en navegación, fetch y XHR",
                    )
                    .changed()
                {
                    changed = true;
                }
                ui.label(
                    RichText::new(
                        "No filtra todos los recursos web y no sustituye un bloqueador de contenido.",
                    )
                        .size(11.0)
                        .color(p.muted),
                );
                ui.add_space(4.0);
                if ui
                    .checkbox(&mut s.freeze_background_media, "Congelar reproducción en segundo plano")
                    .changed()
                {
                    changed = true;
                }
                ui.label(
                    RichText::new("Pausa automáticamente el video y audio en pestañas no activas (excepto YouTube Music).")
                        .size(11.0)
                        .color(p.muted),
                );
                if ui
                    .checkbox(&mut s.auto_engine, "Elegir motor automáticamente por URL")
                    .changed()
                {
                    changed = true;
                }
                ui.label(
                    RichText::new("Abre artículos, wikis y documentación en Wisp (liviano) y el resto en WebView2. Puedes forzar el motor por pestaña con el botón del dial o Ctrl+Shift+W.")
                        .size(11.0)
                        .color(p.muted),
                );

                section(ui, p, "Motor web");
                let previous_default_engine = s.default_engine;
                egui::ComboBox::from_id_salt("default_browser_engine")
                    .selected_text(s.default_engine.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut s.default_engine,
                            BrowserEngine::WebView2,
                            "WebView2 (predeterminado)",
                        );
                        ui.selectable_value(
                            &mut s.default_engine,
                            BrowserEngine::Wisp,
                            "Wisp (Beta)",
                        );
                    });
                changed |= s.default_engine != previous_default_engine;
                ui.label(
                    RichText::new("El motor predeterminado se usa en las pestañas nuevas.")
                        .size(11.0)
                        .color(p.muted),
                );
                if active_tab_id.is_some() {
                    let previous_active_engine = active_engine;
                    egui::ComboBox::from_id_salt("active_tab_engine")
                        .selected_text(format!("Pestaña actual: {}", active_engine.label()))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut active_engine,
                                BrowserEngine::WebView2,
                                "WebView2",
                            );
                            ui.selectable_value(
                                &mut active_engine,
                                BrowserEngine::Wisp,
                                "Wisp (Beta)",
                            );
                        });
                    active_engine_changed = active_engine != previous_active_engine;
                }
                ui.label(
                    RichText::new(
                        "Wisp es experimental: HTML5, CSS básico y JavaScript inline/externo con un DOM limitado; no ejecuta módulos ES ni carga imágenes.",
                    )
                    .size(11.0)
                    .color(p.muted),
                );

                section(ui, p, "Apariencia");
                if ui.checkbox(&mut s.dark_mode, "Modo oscuro").changed() {
                    changed = true;
                }
                ui.add_space(6.0);
                ui.label(RichText::new("Página de inicio").size(12.0).color(p.text));
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut s.home_page)
                            .desired_width(f32::INFINITY)
                            .hint_text("ghost://newtab"),
                    )
                    .changed()
                {
                    changed = true;
                }

                section(ui, p, "Descargas");
                let mut dir = s.download_dir.clone().unwrap_or_default();
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut dir)
                            .desired_width(f32::INFINITY)
                            .hint_text("Carpeta predeterminada"),
                    )
                    .changed()
                {
                    let trimmed = dir.trim().to_string();
                    s.download_dir = if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed)
                    };
                    changed = true;
                }

                section(ui, p, "Privacidad");
                if ui.button("Borrar cookies y datos de navegación").clicked() {
                    clear_data = true;
                }

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "Ghost {} · WebView2 {version}",
                            env!("CARGO_PKG_VERSION")
                        ))
                            .size(10.5)
                            .color(p.muted),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cerrar").clicked() {
                            close = true;
                        }
                    });
                });
            });

            if changed {
                app.set_settings(s);
            }
            if active_engine_changed && let Some(tab_id) = active_tab_id {
                app.set_tab_engine(tab_id, active_engine);
            }
            if clear_data {
                app.clear_browsing_data();
            }
            if close {
                app.panel = Panel::None;
            }
        }
    }
}

// ---------------------------------------------------------------- estado

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

/// Barra inferior: se pinta dentro del mismo área del chrome para que todo el
/// marco comparta el mismo sistema de coordenadas.
fn draw_status_bar(painter: &egui::Painter, screen: Rect, app: &Browser, p: &Palette) {
    let bar = Rect::from_min_size(
        pos2(screen.left(), screen.bottom() - STATUS_HEIGHT),
        vec2(screen.width(), STATUS_HEIGHT),
    );
    painter.rect_filled(bar, 0.0, p.chrome_soft);
    painter.hline(bar.x_range(), bar.top() + 0.5, Stroke::new(1.0, p.border));

    let font = FontId::proportional(11.0);
    let y = bar.center().y;
    painter.text(
        pos2(screen.left() + 12.0, y),
        Align2::LEFT_CENTER,
        app.status(),
        font.clone(),
        p.muted,
    );

    let zoom = app
        .active_tab()
        .map(|t| format!("{:.0}%", t.zoom * 100.0))
        .unwrap_or_else(|| "100%".to_string());
    let tabs = app.tabs.len();
    let suspended = app.suspended_count();
    let right = if app.store.settings.memory_saver {
        format!(
            "{tabs} {} · {suspended} {} · ahorro de memoria · {zoom}",
            plural(tabs, "pestaña", "pestañas"),
            plural(suspended, "suspendida", "suspendidas")
        )
    } else {
        format!("{tabs} {} · {zoom}", plural(tabs, "pestaña", "pestañas"))
    };
    let galley = painter.layout_no_wrap(right, font, p.muted);
    let text_left = bar.right() - 12.0 - galley.size().x;
    painter.galley(pos2(text_left, y - galley.size().y * 0.5), galley, p.muted);
    if app.store.settings.memory_saver {
        painter.circle_filled(pos2(text_left - 8.0, y), 3.0, p.text);
    }
}

/// Accesos rápidos de la nueva pestaña nativa (los mismos que servía la
/// antigua página `ghost://` dentro de WebView2).
const QUICK_LINKS: &[(&str, &str)] = &[
    ("DuckDuckGo", "https://duckduckgo.com"),
    ("GitHub", "https://github.com"),
    ("Wikipedia", "https://es.wikipedia.org"),
    ("docs.rs", "https://docs.rs"),
    ("YouTube", "https://www.youtube.com"),
];

/// Página de nueva pestaña pintada con egui: sin WebView2 ni Wisp, así que
/// abrir Ghost o una pestaña nueva no arranca ningún proceso de Chromium.
fn draw_newtab_page(ui: &mut Ui, app: &mut Browser, p: &Palette) {
    let area = ui.max_rect();
    let search_width = (area.width() * 0.86).clamp(280.0, 560.0);
    let mut query = std::mem::take(&mut app.newtab_query);
    let mut navigate: Option<String> = None;

    // Dibujar imagen de fondo
    let painter = ui.painter();
    if !p.dark {
        // Modo claro: fondo sólido
        painter.rect_filled(area, 0.0, Color32::from_rgb(250, 250, 250));
    } else {
        // Modo oscuro: imagen de fondo con overlay
        if let Some(bg_tex) = background_texture(ui.ctx(), app) {
            painter.image(
                bg_tex.id(),
                area,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        // Overlay oscuro semitransparente
        painter.rect_filled(
            area,
            0.0,
            Color32::from_rgba_premultiplied(0, 0, 0, 158),
        );
    }

    ui.centered_and_justified(|ui| {
        ui.add_space(6.0);
        let (logo_rect, _) = ui.allocate_exact_size(vec2(64.0, 64.0), Sense::hover());
        paint_logo(ui, logo_rect, app);
        ui.add_space(8.0);
        ui.label(RichText::new("Ghost").size(30.0).strong().color(p.text));
        ui.add_space(20.0);

        let response = egui::Frame::NONE
            .fill(p.surface)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(search_width - 26.0);
                ui.add(
                    egui::TextEdit::singleline(&mut query)
                        .hint_text("Buscar en la web o escribir una dirección")
                        .desired_width(f32::INFINITY)
                        .frame(egui::Frame::NONE),
                )
            })
            .inner;
        if ui.memory(|m| m.focused().is_none()) {
            response.request_focus();
        }
        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            let q = query.trim();
            if !q.is_empty() {
                navigate = Some(q.to_string());
            }
        }
        ui.add_space(16.0);

        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for (label, url) in QUICK_LINKS {
                let tile =
                    egui::Button::new(RichText::new(*label).size(13.0).color(p.muted_strong))
                        .fill(Color32::TRANSPARENT)
                        .stroke(Stroke::new(1.0, p.border))
                        .corner_radius(egui::CornerRadius::same(9))
                        .min_size(vec2(0.0, 34.0));
                if ui.add(tile).on_hover_text(*url).clicked() {
                    navigate = Some((*url).to_string());
                }
            }
        });
    });

    app.newtab_query = query;
    if let Some(target) = navigate {
        app.newtab_query.clear();
        app.navigate_active(&target);
    }
}

fn draw_splash(ui: &mut Ui, app: &mut Browser, p: &Palette, area: Rect) {
    let painter = ui.painter().clone();
    let c = area.center();
    if let Some(tex) = logo_texture(ui.ctx(), app) {
        let logo_rect = Rect::from_center_size(pos2(c.x, c.y - 26.0), vec2(88.0, 88.0));
        painter.image(
            tex.id(),
            logo_rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            p.text,
        );
    }
    painter.text(
        pos2(c.x, c.y + 42.0),
        Align2::CENTER_CENTER,
        "Ghost",
        FontId::proportional(24.0),
        p.text,
    );
    painter.text(
        pos2(c.x, c.y + 64.0),
        Align2::CENTER_CENTER,
        "Preparando tu navegador…",
        FontId::proportional(11.5),
        p.muted,
    );
    let phase = (ui.input(|i| i.time) as f32 * 0.8).fract();
    spinner(&painter, pos2(c.x, c.y + 92.0), 8.0, p.accent, phase);
    ui.ctx().request_repaint();
}

fn draw_restore_overlay(ui: &mut Ui, app: &mut Browser, p: &Palette, area: Rect) {
    let Some((tab_id, live, loading)) = app
        .active_tab()
        .map(|tab| (tab.id, tab.is_live(), tab.loading))
    else {
        return;
    };
    let restoring = live && app.is_restoring(tab_id);
    if (live && !restoring) || app.tabs.is_empty() {
        return;
    }
    let painter = ui.painter().clone();
    let c = area.center();
    if let Some(tex) = logo_texture(ui.ctx(), app) {
        let logo_rect = Rect::from_center_size(pos2(c.x, c.y - 34.0), vec2(52.0, 52.0));
        painter.image(
            tex.id(),
            logo_rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            p.text,
        );
    }
    if !live && !loading {
        painter.text(
            pos2(c.x, c.y + 50.0),
            Align2::CENTER_CENTER,
            "No se pudo restaurar la pestaña",
            FontId::proportional(12.5),
            p.muted,
        );
        let retry_rect = Rect::from_center_size(pos2(c.x, c.y + 84.0), vec2(108.0, 28.0));
        if ui
            .put(retry_rect, egui::Button::new("Reintentar"))
            .clicked()
        {
            app.retry_restore(tab_id);
        }
        return;
    }
    let phase = (ui.input(|i| i.time) as f32 * 0.8).fract();
    spinner(&painter, pos2(c.x, c.y + 26.0), 10.0, p.accent, phase);
    painter.text(
        pos2(c.x, c.y + 50.0),
        Align2::CENTER_CENTER,
        "Restaurando pestaña…",
        FontId::proportional(12.5),
        p.muted,
    );
    ui.ctx().request_repaint();
}

/// Atajos de teclado gestionados por winit (cuando el foco está en la UI).
pub fn shortcut_from_key(key: &str, ctrl: bool, alt: bool, shift: bool) -> Option<&'static str> {
    if alt && !ctrl && !shift {
        return match key {
            "ArrowLeft" => Some("back"),
            "ArrowRight" => Some("forward"),
            _ => None,
        };
    }
    if ctrl && !alt {
        let k = key.to_lowercase();
        return match (k.as_str(), shift) {
            ("t", false) => Some("new_tab"),
            ("t", true) => Some("restore_last_closed"),
            ("w", false) => Some("close_tab"),
            ("w", true) => Some("toggle_engine"),
            ("l", false) => Some("focus_url"),
            ("r", false) => Some("reload"),
            ("d", false) => Some("bookmark"),
            ("h", false) => Some("history"),
            ("b", false) => Some("bookmarks"),
            ("=", false) | ("+", false) => Some("zoom_in"),
            ("-", false) => Some("zoom_out"),
            ("0", false) => Some("zoom_reset"),
            ("p", false) => Some("print"),
            ("tab", false) => Some("next_tab"),
            ("tab", true) => Some("prev_tab"),
            _ => None,
        };
    }
    None
}

pub fn key_is_f5(key: &winit::keyboard::Key) -> bool {
    matches!(
        key,
        winit::keyboard::Key::Named(winit::keyboard::NamedKey::F5)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn el_logo_visible_en_tema_claro_y_oscuro() {
        // El splash tiñe la marca blanca con el color del texto: en claro debe
        // quedar oscura sobre el fondo claro, y en oscuro blanca.
        let light = palette(false);
        assert!(light.text.r() < 64);
        assert!(light.bg.r() > 192);
        let dark = palette(true);
        assert!(dark.text.r() > 192);
        assert!(dark.bg.r() < 64);
    }

    #[test]
    fn test_shortcuts() {
        assert_eq!(shortcut_from_key("t", true, false, false), Some("new_tab"));
        assert_eq!(
            shortcut_from_key("t", true, false, true),
            Some("restore_last_closed")
        );
        assert_eq!(
            shortcut_from_key("w", true, false, false),
            Some("close_tab")
        );
    }

    #[test]
    fn external_script_warnings_offer_webview2() {
        assert!(warnings_need_webview(&[
            "Wisp no pudo ejecutar 19 scripts externos.".to_owned()
        ]));
        assert!(warnings_need_webview(&[
            "Wisp no pudo ejecutar 2 módulos ES.".to_owned()
        ]));
        assert!(warnings_need_webview(&[
            "Wisp no pudo ejecutar 1 script externo.".to_owned()
        ]));
        assert!(!warnings_need_webview(&[
            "Se omitió el script inline 1 por superar el límite de tamaño.".to_owned()
        ]));
        assert!(!warnings_need_webview(&[
            "El módulo de interfaz se dibujó correctamente.".to_owned()
        ]));
    }

    #[test]
    fn test_ui_draw_dimensions() {
        let ctx = egui::Context::default();
        let (mut browser, _tx) = Browser::for_tests();
        browser.request_new_tab();
        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 700.0),
            )),
            ..Default::default()
        };
        ctx.begin_pass(raw_input);
        draw(&ctx, &mut browser);
        let mut output = ctx.end_pass();
        output.textures_delta.clear();
    }

    #[test]
    fn el_webview_respeta_el_inset_superior_del_viewport() {
        let ctx = egui::Context::default();
        let (mut browser, _tx) = Browser::for_tests();
        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_max(
                egui::pos2(0.0, 24.0),
                egui::pos2(1000.0, 700.0),
            )),
            ..Default::default()
        };

        ctx.begin_pass(raw_input);
        draw(&ctx, &mut browser);
        ctx.end_pass().textures_delta.clear();

        assert!(
            browser.chrome_height >= 24.0 + UI_HEIGHT - 0.5,
            "la WebView debe comenzar debajo del inset y de todo el chrome: {}",
            browser.chrome_height
        );
    }

    #[test]
    fn el_boton_superior_recibe_el_clic() {
        let ctx = egui::Context::default();
        let (mut browser, _tx) = Browser::for_tests();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0));
        let close_id = egui::Id::unique(("ghost_window_control", 2));
        let first = egui::RawInput {
            screen_rect: Some(screen),
            ..Default::default()
        };
        ctx.begin_pass(first);
        draw(&ctx, &mut browser);
        ctx.end_pass().textures_delta.clear();
        let close_rect = ctx
            .read_response(close_id)
            .expect("el botón de cerrar debe estar registrado")
            .rect;
        let pos = close_rect.center();

        for pressed in [true, false] {
            let raw_input = egui::RawInput {
                screen_rect: Some(screen),
                events: vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::default(),
                    },
                ],
                ..Default::default()
            };
            ctx.begin_pass(raw_input);
            let close_requested = draw(&ctx, &mut browser);
            ctx.end_pass().textures_delta.clear();
            if !pressed {
                assert!(close_requested, "el clic debe activar el control superior");
            }
        }
    }

    #[test]
    fn test_ui_draw_narrow_screens() {
        for width in [400.0, 500.0, 600.0, 800.0, 1920.0] {
            let ctx = egui::Context::default();
            let (mut browser, _tx) = Browser::for_tests();
            browser.request_new_tab();
            let raw_input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 700.0),
                )),
                ..Default::default()
            };
            ctx.begin_pass(raw_input);
            draw(&ctx, &mut browser);
            let mut output = ctx.end_pass();
            output.textures_delta.clear();
        }
    }

    /// El chrome debe repartirse de forma exacta en cualquier ancho: los
    /// controles de ventana pegados al borde derecho, la barra de direcciones
    /// dentro del marco y las pestañas sin invadir a los controles.
    #[test]
    fn el_chrome_se_adapta_a_cualquier_ancho() {
        for w in [420.0, 560.0, 700.0, 1000.0, 1920.0] {
            let ctx = egui::Context::default();
            let (mut browser, _tx) = Browser::for_tests();
            browser.tabs.clear();
            let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w, 700.0));
            for _ in 0..2 {
                let raw = egui::RawInput {
                    screen_rect: Some(rect),
                    ..Default::default()
                };
                ctx.begin_pass(raw);
                draw(&ctx, &mut browser);
                ctx.end_pass().textures_delta.clear();
            }

            let control = |i: usize| {
                ctx.read_response(egui::Id::unique(("ghost_window_control", i)))
                    .map(|r| r.rect)
            };
            let cerrar = control(2).unwrap_or_else(|| panic!("w={w}: falta el botón de cerrar"));
            let minimizar = control(0).unwrap();
            assert!(
                (cerrar.right() - (rect.right() - 6.0)).abs() < 0.5,
                "w={w}: el botón de cerrar debe acabar pegado al borde derecho, acabó en {}",
                cerrar.right()
            );
            assert!(
                minimizar.right() <= cerrar.left(),
                "w={w}: los controles se solapan ({} > {})",
                minimizar.right(),
                cerrar.left()
            );

            let addr = ctx
                .read_response(egui::Id::unique("address_bar"))
                .map(|r| r.rect)
                .unwrap_or_else(|| panic!("w={w}: falta la barra de direcciones"));
            let chrome_bottom = crate::browser::UI_HEIGHT;
            assert!(
                addr.bottom() <= chrome_bottom + 0.5,
                "w={w}: la barra de direcciones sale del chrome ({} > {chrome_bottom})",
                addr.bottom()
            );
            assert!(
                addr.left() >= rect.left() && addr.right() <= rect.right() - 6.0,
                "w={w}: la barra de direcciones se sale de la ventana ({} - {})",
                addr.left(),
                addr.right()
            );
        }
    }

    /// Comprueba que el simulador de clics (mover, pulsar, soltar) llega de
    /// verdad a un widget: si esto falla, no se puede validar el chrome.
    #[test]
    fn el_simulador_de_clics_llega_a_un_widget() {
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 120.0));
        let pos = egui::pos2(100.0, 60.0);
        let mut clicks = 0;
        for events in [
            vec![egui::Event::PointerMoved(pos)],
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            }],
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }],
        ] {
            let raw = egui::RawInput {
                screen_rect: Some(rect),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                let b = egui::Button::new("x").min_size(egui::vec2(80.0, 40.0));
                let area = egui::Rect::from_center_size(pos, egui::vec2(80.0, 40.0));
                if ui.put(area, b).clicked() {
                    clicks += 1;
                }
            });
            output.textures_delta.clear();
        }
        assert_eq!(clicks, 1, "el clic simulado debe registrar un clic");
    }
}
