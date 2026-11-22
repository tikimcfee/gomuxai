//! The agent manager's desktop window: a thin Makepad layer over `agent-manager-core`'s
//! [`Model`]. The model holds every piece of state and decides what each press leads to;
//! this file lays it out, draws its lines, and runs the jobs it asks for off the UI thread.

pub use makepad_widgets;

use std::time::Instant;

use agent_manager_core::agents::{self, AgentLine};
use agent_manager_core::commands::Offer;
use agent_manager_core::data::DataLine;
use agent_manager_core::friction::{self, FrictionLine};
use agent_manager_core::history;
use agent_manager_core::layout::Section;
use agent_manager_core::runner::{self, Output, Source};
use agent_manager_core::sessions;
use agent_manager_core::state::{AUTO_REFRESH_SECS, Dialog, Effect, Job, Model, Page, SortList};
use agent_manager_core::{SessionState, View as FleetView, data};
use chrono::Utc;
use makepad_widgets::*;

app_main!(App);

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    let Cell = Label{
        width: 80
        max_lines: 1
        text_overflow: TextOverflow.Ellipsis
        draw_text +: {
            color: theme.color_text
            text_style: theme.font_regular{font_size: 9}
        }
    }
    let Head = Cell{
        draw_text +: {
            color: theme.color_text_meta
            text_style: theme.font_bold{font_size: 8.5}
        }
    }
    // A column header that sorts the list when clicked: its text (label and sort marker)
    // comes from the model. The width goes on `head`, so it lays out as a row's Cell.
    let HeadCell = View{
        width: Fit
        height: Fit
        cursor: MouseCursor.Hand
        head := Head{}
    }
    // Above a list's header: the order in words, "add columns", remove one, reset.
    let SortBar = View{
        width: Fill
        height: Fit
        flow: Right
        spacing: 8
        padding: Inset{left: 8 right: 8 top: 4 bottom: 0}
        align: Align{y: 0.5}
        Label{
            text: "Sort:"
            draw_text +: {
                color: theme.color_text_meta
                text_style: theme.font_bold{font_size: 8.5}
            }
        }
        sort_words := Label{
            width: Fill
            max_lines: 1
            text_overflow: TextOverflow.Ellipsis
            draw_text +: {
                color: theme.color_text
                text_style: theme.font_regular{font_size: 8.5}
            }
        }
        sort_multi := CheckBox{text: "add columns (or shift-click)"}
        sort_remove := DropDown{labels: ["remove a column…"]}
        sort_reset := ButtonFlat{text: "Default order"}
    }
    let Mono = Label{
        width: Fill
        draw_text +: {
            color: theme.color_text
            text_style: theme.font_code{font_size: 9}
        }
    }
    let Meta = Label{
        width: Fill
        draw_text +: {
            color: theme.color_text_meta
            text_style: theme.font_regular{font_size: 8.5}
        }
    }
    let Bar = View{
        width: Fill
        height: Fit
        flow: Right
        spacing: 8
        padding: Inset{left: 10 right: 10 top: 6 bottom: 6}
        align: Align{y: 0.5}
    }
    // A plain View's background is the stock quad, which draws nothing: what shows a
    // colour is a SolidView.
    let BgBar = SolidView{
        width: Fill
        height: Fit
        flow: Right
        spacing: 8
        padding: Inset{left: 10 right: 10 top: 6 bottom: 6}
        align: Align{y: 0.5}
    }
    let Panel = SolidView{
        height: Fill
        flow: Down
        spacing: 8
        padding: Inset{left: 12 right: 12 top: 10 bottom: 10}
        show_bg: true
        draw_bg +: {color: theme.color_surface_container_low}
    }
    let HostItem = View{
        width: Fill
        height: 30
        flow: Right
        spacing: 10
        padding: Inset{left: 8 right: 8 top: 8}
        align: Align{y: 0.5}
        host := Label{
            draw_text +: {
                color: theme.color_text
                text_style: theme.font_bold{font_size: 10}
            }
        }
        host_note := Meta{width: Fill}
    }
    let NoteItem = View{
        width: Fill
        height: 24
        padding: Inset{left: 24 right: 8}
        align: Align{y: 0.5}
        note := Meta{}
    }
    let Row = SolidView{
        width: Fill
        height: 24
        flow: Right
        spacing: 8
        padding: Inset{left: 8 right: 8}
        align: Align{y: 0.5}
        cursor: MouseCursor.Hand
        show_bg: true
        draw_bg +: {color: #x00000000}
    }

    // --- the shell: sidebar, split panes, detail sections ---
    let SideTab = ButtonFlat{
        width: Fill
        align: Align{x: 0.0 y: 0.5}
        padding: Inset{left: 12 right: 8 top: 7 bottom: 7}
        margin: 0
        draw_text +: {text_style: theme.font_regular{font_size: 10}}
    }
    let SideTabNarrow = SideTab{
        align: Align{x: 0.5 y: 0.5}
        padding: Inset{left: 0 right: 0 top: 7 bottom: 7}
        draw_text +: {text_style: theme.font_bold{font_size: 10}}
    }
    let Side = SolidView{
        height: Fill
        flow: Down
        spacing: 2
        padding: Inset{left: 6 right: 6 top: 8 bottom: 8}
        show_bg: true
        draw_bg +: {color: theme.color_surface_container}
    }
    let VRule = SolidView{
        width: 1
        height: Fill
        show_bg: true
        draw_bg +: {color: theme.color_outline_variant}
    }
    let HRule = SolidView{
        width: Fill
        height: 1
        show_bg: true
        draw_bg +: {color: theme.color_outline_variant}
    }
    // A list beside its detail: the bar between them drags, and a double click on it (or
    // the detail's Hide) folds the detail away.
    let SideSplit = SplitPane{
        width: Fill
        height: Fill
        axis: Horizontal
        align: FromB(272.)
        min_vertical: 260.
        max_vertical: 220.
        fold: mod.widgets.SplitterCollapse.B
        draw_bg +: {color_bg: theme.color_surface_container}
    }
    // A wide list above its detail.
    let StackSplit = SplitPane{
        width: Fill
        height: Fill
        axis: Vertical
        align: FromB(240.)
        min_horizontal: 120.
        max_horizontal: 90.
        fold: mod.widgets.SplitterCollapse.B
        draw_bg +: {color_bg: theme.color_surface_container}
    }
    let ListPane = View{
        width: Fill
        height: Fill
        flow: Down
    }
    let DetailHead = View{
        width: Fill
        height: Fit
        flow: Right
        spacing: 6
        align: Align{y: 0.5}
    }
    let DetailTitle = Label{
        width: Fill
        max_lines: 1
        text_overflow: TextOverflow.Ellipsis
        draw_text +: {text_style: theme.font_bold{font_size: 11}}
    }
    let HideButton = ButtonFlat{text: "Hide ▸"}
    let DetailScroll = ScrollYView{
        width: Fill
        height: Fill
        flow: Down
        spacing: 4
        padding: Inset{right: 10}
    }
    let SectionHead = ButtonFlat{
        width: Fill
        align: Align{x: 0.0 y: 0.5}
        padding: Inset{left: 0 right: 4 top: 6 bottom: 4}
        margin: 0
        draw_text +: {
            color: theme.color_text_meta
            text_style: theme.font_bold{font_size: 9.5}
        }
    }
    let SectionBody = View{
        width: Fill
        height: Fit
        flow: Down
        spacing: 6
        padding: Inset{left: 12 bottom: 6}
    }
    // What is left of a folded detail pane beside the list: the way to bring it back.
    let SideStrip = SolidView{
        visible: false
        width: Fit
        height: Fill
        flow: Down
        padding: Inset{left: 2 right: 2 top: 6}
        show_bg: true
        draw_bg +: {color: theme.color_surface_container_low}
    }
    let StackStrip = SolidView{
        visible: false
        width: Fill
        height: Fit
        flow: Right
        padding: Inset{left: 8 right: 8 top: 2 bottom: 2}
        show_bg: true
        draw_bg +: {color: theme.color_surface_container_low}
    }

    mod.widgets.AgentListBase = #(AgentList::register_widget(vm))
    mod.widgets.AgentList = set_type_default() do mod.widgets.AgentListBase{
        width: Fill
        height: Fill
        flow: Down
        SortBar{}
        Bar{
            padding: Inset{left: 8 right: 8 top: 4 bottom: 4}
            c0 := HeadCell{head +: {width: 176}}
            c1 := HeadCell{head +: {width: 92}}
            c2 := HeadCell{head +: {width: 46}}
            c3 := HeadCell{head +: {width: 104}}
            c4 := HeadCell{head +: {width: 50}}
            c5 := HeadCell{head +: {width: 50}}
            c6 := HeadCell{head +: {width: 40}}
            c7 := HeadCell{width: Fill head +: {width: Fill}}
        }
        list := PortalList{
            width: Fill
            height: Fill
            scan: [@Host, @Note, @Agent]
            Host := HostItem{}
            Note := NoteItem{}
            Agent := View{
                width: Fill
                height: Fit
                row := Row{
                    name := Cell{width: 176}
                    status := Cell{width: 92}
                    backend := Cell{width: 46}
                    model := Cell{width: 104}
                    context := Cell{width: 50}
                    tokens := Cell{width: 50}
                    held := Cell{width: 40}
                    flags := Cell{width: Fill}
                }
            }
        }
    }

    mod.widgets.SessionListBase = #(SessionList::register_widget(vm))
    mod.widgets.SessionList = set_type_default() do mod.widgets.SessionListBase{
        width: Fill
        height: Fill
        flow: Down
        SortBar{}
        Bar{
            spacing: 6
            padding: Inset{left: 8 right: 8 top: 4 bottom: 4}
            // Over the checkboxes: a Head's padding makes it 6 wider than its width.
            Head{width: 16 text: ""}
            c0 := HeadCell{head +: {width: 40}}
            c1 := HeadCell{head +: {width: 62}}
            c2 := HeadCell{head +: {width: 40}}
            c3 := HeadCell{head +: {width: 72}}
            c4 := HeadCell{head +: {width: 90}}
            c5 := HeadCell{head +: {width: 78}}
            c6 := HeadCell{head +: {width: 96}}
            c7 := HeadCell{head +: {width: 96}}
            c8 := HeadCell{head +: {width: 44}}
            c9 := HeadCell{head +: {width: 80}}
            c10 := HeadCell{head +: {width: 60}}
            c11 := HeadCell{width: Fill head +: {width: Fill}}
        }
        list := PortalList{
            width: Fill
            height: Fill
            scan: [@Session, @Empty]
            Empty := NoteItem{}
            Session := View{
                width: Fill
                height: Fit
                row := Row{
                    spacing: 6
                    check := CheckBox{text: "" width: 22}
                    when := Cell{width: 40}
                    host := Cell{width: 62}
                    harness := Cell{width: 40}
                    project := Cell{width: 72}
                    state := Cell{width: 90}
                    status := Cell{width: 78}
                    agent := Cell{width: 96}
                    model := Cell{width: 96}
                    context := Cell{width: 44}
                    tokens := Cell{width: 80}
                    size := Cell{width: 60}
                    title := Cell{width: Fill}
                }
            }
        }
    }

    mod.widgets.FrictionListBase = #(FrictionList::register_widget(vm))
    mod.widgets.FrictionList = set_type_default() do mod.widgets.FrictionListBase{
        width: Fill
        height: Fill
        flow: Down
        SortBar{}
        Bar{
            padding: Inset{left: 8 right: 8 top: 4 bottom: 4}
            c0 := HeadCell{head +: {width: 40}}
            c1 := HeadCell{head +: {width: 60}}
            c2 := HeadCell{head +: {width: 64}}
            c3 := HeadCell{head +: {width: 46}}
            c4 := HeadCell{width: Fill head +: {width: Fill}}
            c5 := HeadCell{head +: {width: 100}}
        }
        list := PortalList{
            width: Fill
            height: Fill
            scan: [@Host, @Note, @Entry]
            Host := HostItem{}
            Note := NoteItem{}
            Entry := View{
                width: Fill
                height: Fit
                row := Row{
                    id := Cell{width: 40}
                    kind := Cell{width: 60}
                    status := Cell{width: 64}
                    votes := Cell{width: 46}
                    summary := Cell{width: Fill}
                    links := Cell{width: 100}
                }
            }
        }
    }

    mod.widgets.HistoryListBase = #(HistoryList::register_widget(vm))
    mod.widgets.HistoryList = set_type_default() do mod.widgets.HistoryListBase{
        width: Fill
        height: Fill
        flow: Down
        SortBar{}
        Bar{
            padding: Inset{left: 8 right: 8 top: 4 bottom: 4}
            c0 := HeadCell{head +: {width: 40}}
            c1 := HeadCell{head +: {width: 80}}
            c2 := HeadCell{head +: {width: 150}}
            c3 := HeadCell{width: Fill head +: {width: Fill}}
            c4 := HeadCell{head +: {width: 44}}
            c5 := HeadCell{head +: {width: 60}}
        }
        list := PortalList{
            width: Fill
            height: Fill
            scan: [@Line, @Empty]
            Empty := NoteItem{}
            Line := View{
                width: Fill
                height: Fit
                row := Row{
                    when := Cell{width: 40}
                    host := Cell{width: 80}
                    who := Cell{width: 150}
                    command := Cell{
                        width: Fill
                        draw_text +: {text_style: theme.font_code{font_size: 8.5}}
                    }
                    exit := Cell{width: 44}
                    took := Cell{width: 60}
                }
            }
        }
    }

    // The data page: a section per host (header, then headings and name/value rows with
    // a bar for each value's share of its group's largest), folded by pressing the header.
    mod.widgets.DataListBase = #(DataList::register_widget(vm))
    mod.widgets.DataList = set_type_default() do mod.widgets.DataListBase{
        width: Fill
        height: Fill
        flow: Down
        list := PortalList{
            width: Fill
            height: Fill
            scan: [@Host, @Heading, @Item, @Note, @Empty]
            Empty := NoteItem{}
            Host := View{
                width: Fill
                height: Fit
                padding: Inset{top: 10}
                row := Row{
                    height: 32
                    spacing: 8
                    show_bg: true
                    draw_bg +: {color: theme.color_surface_container}
                    fold := Label{
                        width: 14
                        draw_text +: {
                            color: theme.color_text_meta
                            text_style: theme.font_bold{font_size: 10}
                        }
                    }
                    host := Label{
                        draw_text +: {
                            color: theme.color_text
                            text_style: theme.font_bold{font_size: 11}
                        }
                    }
                    host_note := Meta{
                        width: Fill
                        max_lines: 1
                        text_overflow: TextOverflow.Ellipsis
                    }
                    here := Meta{width: Fit}
                }
            }
            Heading := View{
                width: Fill
                height: 28
                flow: Right
                spacing: 10
                padding: Inset{left: 30 right: 8 top: 8}
                align: Align{y: 0.5}
                title := Label{
                    width: 150
                    draw_text +: {
                        color: theme.color_text
                        text_style: theme.font_bold{font_size: 9.5}
                    }
                }
                summary := Meta{
                    width: Fill
                    max_lines: 1
                    text_overflow: TextOverflow.Ellipsis
                }
            }
            Item := View{
                width: Fill
                height: 20
                flow: Right
                spacing: 10
                padding: Inset{left: 46 right: 8}
                align: Align{y: 0.5}
                name := Cell{width: 134}
                value := Cell{
                    width: 84
                    draw_text +: {text_style: theme.font_code{font_size: 8.5}}
                }
                well := SolidView{
                    width: 120
                    height: 8
                    flow: Right
                    show_bg: true
                    draw_bg +: {color: theme.color_surface_container}
                    bar := SolidView{
                        width: 0
                        height: Fill
                        show_bg: true
                        draw_bg +: {color: #x6cb6ff80}
                    }
                }
                detail := Cell{
                    width: Fill
                    draw_text +: {color: theme.color_text_meta}
                }
            }
            Note := NoteItem{padding: Inset{left: 46 right: 8}}
        }
    }

    startup() do #(App::script_component(vm)){
        ui: Root{
            main_window := Window{
                window.inner_size: vec2(1100, 700)
                window.title: "agent manager"
                body +: {
                    width: Fill
                    height: Fill
                    flow: Down
                    spacing: 0

                    shell := View{
                    width: Fill
                    height: Fill
                    flow: Right

                    side_wide := Side{
                        width: 148
                        DetailHead{
                            padding: Inset{left: 6 bottom: 6}
                            Label{
                                width: Fill
                                max_lines: 1
                                text: "agent manager"
                                draw_text +: {text_style: theme.font_bold{font_size: 10}}
                            }
                            side_fold := ButtonFlat{text: "«"}
                        }
                        tab_agents := SideTab{text: "Agents"}
                        tab_sessions := SideTab{text: "Sessions"}
                        tab_friction := SideTab{text: "Friction"}
                        tab_history := SideTab{text: "History"}
                        tab_data := SideTab{text: "Data"}
                        Filler{}
                        auto := CheckBox{text: "auto refresh 10s" active: true}
                        refresh := Button{width: Fill text: "Refresh"}
                    }
                    side_narrow := Side{
                        visible: false
                        width: 48
                        padding: Inset{left: 4 right: 4 top: 8 bottom: 8}
                        side_unfold := SideTabNarrow{text: "»"}
                        ntab_agents := SideTabNarrow{text: "A"}
                        ntab_sessions := SideTabNarrow{text: "S"}
                        ntab_friction := SideTabNarrow{text: "F"}
                        ntab_history := SideTabNarrow{text: "H"}
                        ntab_data := SideTabNarrow{text: "D"}
                        Filler{}
                        nauto := CheckBox{text: "" active: true}
                        nrefresh := Button{width: Fill text: "↻"}
                    }
                    VRule{}

                    pages := View{
                    width: Fill
                    height: Fill
                    flow: Down

                    agents_page := View{
                        width: Fill
                        height: Fill
                        flow: Right
                        agent_split := SideSplit{
                            a: ListPane{
                                agent_list := mod.widgets.AgentList{}
                            }
                            b: Panel{
                                width: Fill
                                DetailHead{
                                    agent_title := DetailTitle{text: "Select an agent"}
                                    agent_hide := HideButton{}
                                }
                                Bar{
                                    padding: 0
                                    resume := Button{text: "Resume"}
                                    revive := Button{text: "Revive"}
                                    cleanup := Button{text: "Clean up…"}
                                }
                                HRule{}
                                DetailScroll{
                                    sec_agent_detail := SectionHead{}
                                    sec_agent_detail_body := SectionBody{
                                        agent_why := Meta{}
                                        agent_detail := Mono{}
                                    }
                                    sec_agent_logs := SectionHead{}
                                    sec_agent_logs_body := SectionBody{
                                        agent_logs := Mono{
                                            draw_text +: {text_style: theme.font_code{font_size: 8}}
                                        }
                                    }
                                }
                            }
                        }
                        agent_strip := SideStrip{
                            agent_show := ButtonFlat{text: "◂"}
                        }
                    }

                    sessions_page := View{
                        visible: false
                        width: Fill
                        height: Fill
                        flow: Down
                        Bar{
                            filter_text := TextInput{
                                width: 240
                                empty_text: "filter: title, project, agent, id"
                            }
                            host_dd := DropDown{labels: ["all hosts"]}
                            harness_dd := DropDown{labels: ["all harnesses"]}
                            state_dd := DropDown{labels: ["all states" "running" "dormant" "ended" "missing" "unknown"]}
                            stale_cb := CheckBox{text: "stale only"}
                            untracked_cb := CheckBox{text: "show untracked"}
                        }
                        session_split := StackSplit{
                            a: ListPane{
                                session_list := mod.widgets.SessionList{}
                            }
                            b: Panel{
                                width: Fill
                                DetailHead{
                                    session_title := DetailTitle{text: "Session"}
                                    session_hide := HideButton{text: "Hide ▾"}
                                }
                                HRule{}
                                DetailScroll{
                                    sec_session_detail := SectionHead{}
                                    sec_session_detail_body := SectionBody{
                                        session_detail := Mono{}
                                    }
                                    sec_session_tokens := SectionHead{}
                                    sec_session_tokens_body := SectionBody{
                                        session_tokens := Mono{}
                                    }
                                }
                            }
                        }
                        session_strip := StackStrip{
                            session_show := ButtonFlat{text: "▴ Show detail"}
                        }
                        BgBar{
                            show_bg: true
                            draw_bg +: {color: theme.color_surface_container}
                            totals := Label{
                                width: Fill
                                max_lines: 1
                                text_overflow: TextOverflow.Ellipsis
                            }
                            selected := Meta{width: Fit}
                            select_all := ButtonFlat{text: "Select shown"}
                            clear_sel := ButtonFlat{text: "Clear"}
                            track := Button{text: "Track again"}
                            untrack := Button{text: "Stop tracking…"}
                            untrack_all := Button{text: "Stop tracking all shown not running…"}
                        }
                    }

                    friction_page := View{
                        visible: false
                        width: Fill
                        height: Fill
                        flow: Right
                        friction_split := SideSplit{
                            align: FromB(400.)
                            a: ListPane{
                                friction_list := mod.widgets.FrictionList{}
                            }
                            b: Panel{
                                width: Fill
                                DetailHead{
                                    friction_title := DetailTitle{text: "Select an entry"}
                                    friction_hide := HideButton{}
                                }
                                HRule{}
                                DetailScroll{
                                    sec_friction_detail := SectionHead{}
                                    sec_friction_detail_body := SectionBody{
                                        friction_detail := Label{
                                            width: Fill
                                            text: "Select an entry"
                                            draw_text +: {text_style: theme.font_regular{font_size: 9.5}}
                                        }
                                    }
                                    sec_friction_resolve := SectionHead{}
                                    sec_friction_resolve_body := SectionBody{
                                        friction_note := TextInput{
                                            width: Fill
                                            empty_text: "how it was resolved"
                                        }
                                        Bar{
                                            padding: 0
                                            resolve := Button{text: "Resolve"}
                                            friction_why := Meta{}
                                        }
                                    }
                                }
                            }
                        }
                        friction_strip := SideStrip{
                            friction_show := ButtonFlat{text: "◂"}
                        }
                    }

                    history_page := View{
                        visible: false
                        width: Fill
                        height: Fill
                        flow: Down
                        Bar{
                            history_text := TextInput{
                                width: 320
                                empty_text: "filter: command, who, host, error"
                            }
                            failed_cb := CheckBox{text: "failed only"}
                            history_count := Meta{}
                        }
                        history_split := StackSplit{
                            align: FromB(200.)
                            a: ListPane{
                                history_list := mod.widgets.HistoryList{}
                            }
                            b: Panel{
                                width: Fill
                                DetailHead{
                                    history_title := DetailTitle{text: "Command"}
                                    history_hide := HideButton{text: "Hide ▾"}
                                }
                                HRule{}
                                DetailScroll{
                                    sec_history_detail := SectionHead{}
                                    sec_history_detail_body := SectionBody{
                                        history_detail := Mono{}
                                    }
                                }
                            }
                        }
                        history_strip := StackStrip{
                            history_show := ButtonFlat{text: "▴ Show detail"}
                        }
                    }

                    data_page := View{
                        visible: false
                        width: Fill
                        height: Fill
                        flow: Down
                        Bar{
                            backup := Button{text: "Back up now"}
                            Label{text: "Prune older than"}
                            prune_days := TextInput{width: 50 text: "30"}
                            Label{text: "days"}
                            prune := Button{text: "Prune…"}
                            data_why := Meta{}
                        }
                        data_list := mod.widgets.DataList{}
                    }
                    }
                    }

                    status_bar := BgBar{
                        show_bg: true
                        draw_bg +: {color: theme.color_surface_container_high}
                        status := Meta{
                            max_lines: 1
                            text_overflow: TextOverflow.Ellipsis
                        }
                        source := Meta{width: Fit}
                    }

                    dialog := Modal{
                        content: SolidView{
                            width: 600
                            height: Fit
                            flow: Down
                            spacing: 10
                            padding: Inset{left: 18 right: 18 top: 16 bottom: 16}
                            show_bg: true
                            draw_bg +: {color: theme.color_surface_container_high}
                            dialog_title := Label{
                                width: Fill
                                draw_text +: {text_style: theme.font_bold{font_size: 11}}
                            }
                            ScrollYView{
                                width: Fill
                                height: 320
                                dialog_body := Mono{}
                            }
                            View{
                                width: Fill
                                height: Fit
                                flow: Right
                                spacing: 8
                                Filler{}
                                dialog_no := ButtonFlat{text: "Cancel"}
                                dialog_yes := Button{text: "Confirm"}
                            }
                        }
                    }
                }
            }
        }
    }
}

/// What a background job hands back to the UI thread.
#[derive(Debug)]
enum Msg {
    View(Result<FleetView, String>),
    Done(Job, Output),
}

fn hex(c: u32) -> Vec4f {
    let f = |s: u32| ((c >> s) & 0xff) as f32 / 255.0;
    vec4f(f(24), f(16), f(8), f(0))
}

const SELECTED: u32 = 0x3b82f640;
const CLEAR: u32 = 0x00000000;
const TEXT: u32 = 0xd8dde6ff;
const META: u32 = 0x8b93a1ff;
const GOOD: u32 = 0x5fd38dff;
const BUSY: u32 = 0x6cb6ffff;
const WARN: u32 = 0xf2b84bff;
const BAD: u32 = 0xff6b6bff;

fn status_color(status: &str) -> u32 {
    match status {
        "idle" => GOOD,
        "processing" => BUSY,
        "waiting" => WARN,
        "error" => BAD,
        "unknown" => META,
        _ if status.starts_with("ended") => META,
        _ => TEXT,
    }
}

fn state_color(s: SessionState) -> u32 {
    match s {
        SessionState::Running => GOOD,
        SessionState::Dormant => WARN,
        SessionState::Missing => BAD,
        SessionState::Ended | SessionState::Unknown => META,
    }
}

fn set_color(cx: &mut Cx, label: LabelRef, color: u32) {
    let mut w: WidgetRef = (*label).clone();
    let c = hex(color);
    script_apply_eval!(cx, w, { draw_text +: {color: #(c)} });
}

/// Makepad's `ButtonRef::set_enabled(false)` stops input but keeps the enabled look, and
/// `set_disabled(true)` greys it but still takes presses: a button that is off needs both.
trait Enable {
    fn enable(&self, cx: &mut Cx, on: bool);
}

impl Enable for ButtonRef {
    fn enable(&self, cx: &mut Cx, on: bool) {
        self.set_enabled(cx, on);
        if self.disabled(cx) == on {
            self.set_disabled(cx, !on);
        }
    }
}

fn set_bg(cx: &mut Cx, w: &WidgetRef, color: u32) {
    let mut w = w.clone();
    let c = hex(color);
    script_apply_eval!(cx, w, { draw_bg +: {color: #(c)} });
}

// --- the shell ------------------------------------------------------------------------

type Ids = &'static [LiveId];

/// Each page with a detail pane: its splitter, the strip that brings a folded pane back,
/// and the buttons that hide and show it.
fn details() -> [(Page, Ids, Ids, Ids, Ids); 4] {
    [
        (
            Page::Agents,
            ids!(agent_split),
            ids!(agent_strip),
            ids!(agent_hide),
            ids!(agent_show),
        ),
        (
            Page::Sessions,
            ids!(session_split),
            ids!(session_strip),
            ids!(session_hide),
            ids!(session_show),
        ),
        (
            Page::Friction,
            ids!(friction_split),
            ids!(friction_strip),
            ids!(friction_hide),
            ids!(friction_show),
        ),
        (
            Page::History,
            ids!(history_split),
            ids!(history_strip),
            ids!(history_hide),
            ids!(history_show),
        ),
    ]
}

/// Each foldable section: its header button and its body.
fn sections() -> [(Section, Ids, Ids); 7] {
    [
        (
            Section::AgentDetail,
            ids!(sec_agent_detail),
            ids!(sec_agent_detail_body),
        ),
        (
            Section::AgentLogs,
            ids!(sec_agent_logs),
            ids!(sec_agent_logs_body),
        ),
        (
            Section::SessionDetail,
            ids!(sec_session_detail),
            ids!(sec_session_detail_body),
        ),
        (
            Section::SessionTokens,
            ids!(sec_session_tokens),
            ids!(sec_session_tokens_body),
        ),
        (
            Section::FrictionDetail,
            ids!(sec_friction_detail),
            ids!(sec_friction_detail_body),
        ),
        (
            Section::FrictionResolve,
            ids!(sec_friction_resolve),
            ids!(sec_friction_resolve_body),
        ),
        (
            Section::HistoryDetail,
            ids!(sec_history_detail),
            ids!(sec_history_detail_body),
        ),
    ]
}

// --- sorting by column headers ---------------------------------------------------------

/// The lists sorted by their headers, and their widgets.
fn sort_lists() -> [(SortList, &'static [LiveId]); 4] {
    [
        (SortList::Agents, ids!(agent_list)),
        (SortList::Sessions, ids!(session_list)),
        (SortList::Friction, ids!(friction_list)),
        (SortList::History, ids!(history_list)),
    ]
}

/// A list's header cells, left to right (column `i` is `c{i}`).
fn head_ids() -> [&'static [LiveId]; 12] {
    [
        ids!(c0),
        ids!(c1),
        ids!(c2),
        ids!(c3),
        ids!(c4),
        ids!(c5),
        ids!(c6),
        ids!(c7),
        ids!(c8),
        ids!(c9),
        ids!(c10),
        ids!(c11),
    ]
}

/// The "remove a column…" menu: a title, then each key.
fn remove_labels(m: &Model, list: SortList) -> Vec<String> {
    let mut labels = vec!["remove a column…".to_string()];
    labels.extend(
        m.order(list)
            .key_words()
            .into_iter()
            .map(|w| format!("✕ {w}")),
    );
    labels
}

// --- the lists ------------------------------------------------------------------------

#[derive(Script, ScriptHook, Widget)]
pub struct AgentList {
    #[deref]
    view: View,
}

impl Widget for AgentList {
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        while let Some(step) = self.view.draw_walk(cx, scope, walk).step() {
            let Some(mut list) = step.borrow_mut::<PortalList>() else {
                continue;
            };
            let Some(m) = scope.data.get::<Model>() else {
                continue;
            };
            list.set_item_range(cx, 0, m.agent_lines.len());
            let selected = m.agent();
            while let Some(i) = list.next_visible_item(cx) {
                let Some(line) = m.agent_lines.get(i) else {
                    continue;
                };
                match line {
                    AgentLine::Host { host } => {
                        let h = &m.view.hosts[*host];
                        let item = list.item(cx, i, id!(Host));
                        item.label(cx, ids!(host)).set_text(cx, &h.host);
                        let note = if h.reachable {
                            let n = h.agents.len();
                            format!("{n} agent{} · {}", if n == 1 { "" } else { "s" }, h.build)
                        } else {
                            String::new()
                        };
                        item.label(cx, ids!(host_note)).set_text(cx, &note);
                        item.draw_all_unscoped(cx);
                    }
                    AgentLine::Note { text, .. } => {
                        let item = list.item(cx, i, id!(Note));
                        item.label(cx, ids!(note)).set_text(cx, text);
                        item.draw_all_unscoped(cx);
                    }
                    AgentLine::Agent { host, agent, depth } => {
                        let c = agents::cells(&m.view, *host, *agent, *depth);
                        let item = list.item(cx, i, id!(Agent));
                        item.label(cx, ids!(name)).set_text(cx, &c.name);
                        let status = item.label(cx, ids!(status));
                        status.set_text(cx, &c.status);
                        set_color(cx, status, status_color(&c.status));
                        item.label(cx, ids!(backend)).set_text(cx, &c.backend);
                        item.label(cx, ids!(model)).set_text(cx, &c.model);
                        item.label(cx, ids!(context)).set_text(cx, &c.context);
                        item.label(cx, ids!(tokens)).set_text(cx, &c.tokens);
                        item.label(cx, ids!(held)).set_text(cx, &c.held);
                        let mut tail = Vec::new();
                        if !c.remote_parent.is_empty() {
                            tail.push(format!("from {}", c.remote_parent));
                        }
                        if !c.flags.is_empty() {
                            tail.push(c.flags.clone());
                        }
                        tail.push(c.cwd.clone());
                        let flags = item.label(cx, ids!(flags));
                        flags.set_text(cx, &tail.join(" · "));
                        set_color(cx, flags, if c.flags.is_empty() { META } else { WARN });
                        let on = selected == Some((*host, *agent));
                        let row = item.widget(cx, ids!(row));
                        set_bg(cx, &row, if on { SELECTED } else { CLEAR });
                        item.draw_all_unscoped(cx);
                    }
                }
            }
        }
        DrawStep::done()
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
    }
}

#[derive(Script, ScriptHook, Widget)]
pub struct SessionList {
    #[deref]
    view: View,
}

impl Widget for SessionList {
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let now = Utc::now();
        while let Some(step) = self.view.draw_walk(cx, scope, walk).step() {
            let Some(mut list) = step.borrow_mut::<PortalList>() else {
                continue;
            };
            let Some(m) = scope.data.get::<Model>() else {
                continue;
            };
            if m.session_lines.is_empty() {
                list.set_item_range(cx, 0, 1);
                while let Some(i) = list.next_visible_item(cx) {
                    let item = list.item(cx, i, id!(Empty));
                    let text = if m.read_at.is_none() {
                        "reading…"
                    } else {
                        "no session matches the filters"
                    };
                    item.label(cx, ids!(note)).set_text(cx, text);
                    item.draw_all_unscoped(cx);
                }
                continue;
            }
            list.set_item_range(cx, 0, m.session_lines.len());
            let focused = m.focused_line().map(|l| (l.host, l.index));
            while let Some(i) = list.next_visible_item(cx) {
                let Some(line) = m.session_lines.get(i) else {
                    continue;
                };
                let c = sessions::cells(&m.view, line, now);
                let s = line.row(&m.view);
                let item = list.item(cx, i, id!(Session));
                item.check_box(cx, ids!(check))
                    .set_active(cx, m.is_selected(line), Animate::No);
                item.label(cx, ids!(when)).set_text(cx, &c.when);
                item.label(cx, ids!(host)).set_text(cx, &c.host);
                item.label(cx, ids!(harness)).set_text(cx, &c.harness);
                item.label(cx, ids!(project)).set_text(cx, &c.project);
                let state = item.label(cx, ids!(state));
                state.set_text(cx, &c.state);
                set_color(cx, state, state_color(s.state));
                let status = item.label(cx, ids!(status));
                status.set_text(cx, &c.status);
                set_color(cx, status, status_color(&s.status));
                item.label(cx, ids!(agent)).set_text(cx, &c.agent);
                item.label(cx, ids!(model)).set_text(cx, &c.model);
                item.label(cx, ids!(context)).set_text(cx, &c.context);
                item.label(cx, ids!(tokens)).set_text(cx, &c.tokens);
                item.label(cx, ids!(size)).set_text(cx, &c.size);
                let title = item.label(cx, ids!(title));
                title.set_text(cx, &c.title);
                set_color(cx, title, if line.depth > 0 { META } else { TEXT });
                let bg = if focused == Some((line.host, line.index)) {
                    0x3b82f660
                } else if m.is_selected(line) {
                    SELECTED
                } else {
                    CLEAR
                };
                let row = item.widget(cx, ids!(row));
                set_bg(cx, &row, bg);
                item.draw_all_unscoped(cx);
            }
        }
        DrawStep::done()
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
    }
}

#[derive(Script, ScriptHook, Widget)]
pub struct FrictionList {
    #[deref]
    view: View,
}

impl Widget for FrictionList {
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        while let Some(step) = self.view.draw_walk(cx, scope, walk).step() {
            let Some(mut list) = step.borrow_mut::<PortalList>() else {
                continue;
            };
            let Some(m) = scope.data.get::<Model>() else {
                continue;
            };
            list.set_item_range(cx, 0, m.friction_lines.len());
            let selected = m.friction();
            while let Some(i) = list.next_visible_item(cx) {
                let Some(line) = m.friction_lines.get(i) else {
                    continue;
                };
                match line {
                    FrictionLine::Host { host, open } => {
                        let item = list.item(cx, i, id!(Host));
                        item.label(cx, ids!(host))
                            .set_text(cx, &m.view.hosts[*host].host);
                        item.label(cx, ids!(host_note))
                            .set_text(cx, &format!("{open} open"));
                        item.draw_all_unscoped(cx);
                    }
                    FrictionLine::Note { text, .. } => {
                        let item = list.item(cx, i, id!(Note));
                        item.label(cx, ids!(note)).set_text(cx, text);
                        item.draw_all_unscoped(cx);
                    }
                    FrictionLine::Entry { host, index } => {
                        let f = &m.view.hosts[*host].friction[*index];
                        let c = friction::cells(f);
                        let item = list.item(cx, i, id!(Entry));
                        item.label(cx, ids!(id)).set_text(cx, &c.id);
                        item.label(cx, ids!(kind)).set_text(cx, &c.kind);
                        let status = item.label(cx, ids!(status));
                        status.set_text(cx, &c.status);
                        set_color(cx, status, if friction::is_open(f) { WARN } else { META });
                        item.label(cx, ids!(votes)).set_text(cx, &c.votes);
                        let summary = item.label(cx, ids!(summary));
                        summary.set_text(cx, &c.summary);
                        set_color(cx, summary, if friction::is_open(f) { TEXT } else { META });
                        item.label(cx, ids!(links)).set_text(cx, &c.links);
                        let on = selected == Some((*host, *index));
                        let row = item.widget(cx, ids!(row));
                        set_bg(cx, &row, if on { SELECTED } else { CLEAR });
                        item.draw_all_unscoped(cx);
                    }
                }
            }
        }
        DrawStep::done()
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
    }
}

#[derive(Script, ScriptHook, Widget)]
pub struct HistoryList {
    #[deref]
    view: View,
}

impl Widget for HistoryList {
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let now = Utc::now();
        while let Some(step) = self.view.draw_walk(cx, scope, walk).step() {
            let Some(mut list) = step.borrow_mut::<PortalList>() else {
                continue;
            };
            let Some(m) = scope.data.get::<Model>() else {
                continue;
            };
            if m.history_lines.is_empty() {
                list.set_item_range(cx, 0, 1);
                while let Some(i) = list.next_visible_item(cx) {
                    let item = list.item(cx, i, id!(Empty));
                    item.label(cx, ids!(note))
                        .set_text(cx, "no command matches");
                    item.draw_all_unscoped(cx);
                }
                continue;
            }
            list.set_item_range(cx, 0, m.history_lines.len());
            let shown = m.history_line();
            while let Some(i) = list.next_visible_item(cx) {
                let Some(line) = m.history_lines.get(i) else {
                    continue;
                };
                let c = history::cells(&m.view, line, now);
                let item = list.item(cx, i, id!(Line));
                item.label(cx, ids!(when)).set_text(cx, &c.when);
                item.label(cx, ids!(host)).set_text(cx, &c.host);
                item.label(cx, ids!(who)).set_text(cx, &c.who);
                let command = item.label(cx, ids!(command));
                let text = if c.failed && !c.error.is_empty() {
                    format!("{}   ✗ {}", c.command, c.error)
                } else {
                    c.command.clone()
                };
                command.set_text(cx, &text);
                set_color(cx, command, if c.failed { BAD } else { TEXT });
                let exit = item.label(cx, ids!(exit));
                exit.set_text(cx, &c.exit);
                set_color(cx, exit, if c.failed { BAD } else { META });
                item.label(cx, ids!(took)).set_text(cx, &c.duration);
                let row = item.widget(cx, ids!(row));
                set_bg(
                    cx,
                    &row,
                    if shown == Some(*line) {
                        SELECTED
                    } else {
                        CLEAR
                    },
                );
                item.draw_all_unscoped(cx);
            }
        }
        DrawStep::done()
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
    }
}

/// The data page's list. Which hosts are folded is the page's own state: pressing a host's
/// header toggles it here, and the lines are worked out again on each draw.
#[derive(Script, ScriptHook, Widget)]
pub struct DataList {
    #[deref]
    view: View,
    #[rust]
    folded: std::collections::BTreeSet<String>,
    /// The lines last drawn, to map a pressed item back to its host.
    #[rust]
    lines: Vec<DataLine>,
}

/// The width of a value's bar at a full share.
const DATA_BAR: f64 = 120.0;

impl Widget for DataList {
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let now = Utc::now();
        while let Some(step) = self.view.draw_walk(cx, scope, walk).step() {
            let Some(mut list) = step.borrow_mut::<PortalList>() else {
                continue;
            };
            let Some(m) = scope.data.get::<Model>() else {
                continue;
            };
            self.lines = data::lines(&m.view, now, &self.folded);
            if self.lines.is_empty() {
                list.set_item_range(cx, 0, 1);
                while let Some(i) = list.next_visible_item(cx) {
                    let item = list.item(cx, i, id!(Empty));
                    let text = if m.read_at.is_none() {
                        "reading…"
                    } else {
                        "no host answered"
                    };
                    item.label(cx, ids!(note)).set_text(cx, text);
                    item.draw_all_unscoped(cx);
                }
                continue;
            }
            list.set_item_range(cx, 0, self.lines.len());
            while let Some(i) = list.next_visible_item(cx) {
                let Some(line) = self.lines.get(i) else {
                    continue;
                };
                match line {
                    DataLine::Host {
                        name,
                        note,
                        reachable,
                        folded,
                        local,
                        ..
                    } => {
                        let item = list.item(cx, i, id!(Host));
                        item.label(cx, ids!(fold))
                            .set_text(cx, if *folded { "▸" } else { "▾" });
                        item.label(cx, ids!(host)).set_text(cx, name);
                        let n = item.label(cx, ids!(host_note));
                        n.set_text(cx, note);
                        set_color(cx, n, if *reachable { META } else { BAD });
                        item.label(cx, ids!(here)).set_text(
                            cx,
                            if *local {
                                "this host: Back up and Prune act here"
                            } else {
                                ""
                            },
                        );
                        item.draw_all_unscoped(cx);
                    }
                    DataLine::Heading { title, summary } => {
                        let item = list.item(cx, i, id!(Heading));
                        item.label(cx, ids!(title)).set_text(cx, title);
                        item.label(cx, ids!(summary)).set_text(cx, summary);
                        item.draw_all_unscoped(cx);
                    }
                    DataLine::Item {
                        name,
                        value,
                        detail,
                        share,
                        top,
                    } => {
                        let item = list.item(cx, i, id!(Item));
                        item.label(cx, ids!(name)).set_text(cx, name);
                        let v = item.label(cx, ids!(value));
                        v.set_text(cx, &format!("{value:>10}"));
                        set_color(cx, v, if *top { WARN } else { TEXT });
                        // No share, no bar: the well stays, unpainted, to keep the column.
                        let well = item.widget(cx, ids!(well));
                        set_bg(cx, &well, if share.is_some() { 0x5b5b5bff } else { CLEAR });
                        let share = share.unwrap_or(0.0);
                        let mut bar = item.widget(cx, ids!(bar));
                        // A value above zero shows at least a sliver.
                        let w = if share > 0.0 {
                            (f64::from(share) * DATA_BAR).max(2.0)
                        } else {
                            0.0
                        };
                        script_apply_eval!(cx, bar, { width: #(w) });
                        set_bg(cx, &bar, if *top { 0xf2b84bc0 } else { 0x6cb6ff80 });
                        item.label(cx, ids!(detail)).set_text(cx, detail);
                        item.draw_all_unscoped(cx);
                    }
                    DataLine::Note { text } => {
                        let item = list.item(cx, i, id!(Note));
                        item.label(cx, ids!(note)).set_text(cx, text);
                        item.draw_all_unscoped(cx);
                    }
                }
            }
        }
        DrawStep::done()
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        let actions = cx.capture_actions(|cx| self.view.handle_event(cx, event, scope));
        let list = self.view.portal_list(cx, ids!(list));
        for (i, item) in list.items_with_actions(&actions) {
            let pressed = item
                .view(cx, ids!(row))
                .finger_up(&actions)
                .is_some_and(|e| e.is_over);
            if let (true, Some(DataLine::Host { name, .. })) = (pressed, self.lines.get(i))
                && !self.folded.remove(name)
            {
                self.folded.insert(name.clone());
            }
            if pressed {
                self.view.redraw(cx);
            }
        }
        cx.extend_actions(actions);
    }
}

// --- the app --------------------------------------------------------------------------

#[derive(Script, ScriptHook)]
pub struct App {
    #[live]
    ui: WidgetRef,
    #[rust]
    model: Model,
    #[rust]
    source: Option<Source>,
    #[rust]
    tick: Timer,
    #[rust]
    last_read: Option<Instant>,
    /// The host and harness filter's labels, to set them only when they change.
    #[rust]
    host_labels: Vec<String>,
    #[rust]
    harness_labels: Vec<String>,
    /// Each sorted list's "remove a column…" labels, in `sort_lists()` order.
    #[rust]
    sort_remove_labels: [Vec<String>; 4],
}

impl App {
    fn source(&mut self) -> Source {
        self.source.get_or_insert_with(Source::from_env).clone()
    }

    fn refresh(&mut self, cx: &mut Cx) {
        if !self.model.begin_refresh() {
            return;
        }
        self.last_read = Some(Instant::now());
        let source = self.source();
        runner::spawn(
            move || runner::read_view(&source),
            |r| Cx::post_action(Msg::View(r)),
        );
        self.sync(cx);
    }

    fn apply(&mut self, cx: &mut Cx, effect: Effect) {
        match effect {
            Effect::None => {}
            Effect::Refresh => self.refresh(cx),
            Effect::Run(job) => {
                let agent = self.source().agent;
                runner::spawn(
                    move || {
                        let out = runner::run(agent.as_deref(), &job.command);
                        (job, out)
                    },
                    |(job, out)| Cx::post_action(Msg::Done(job, out)),
                );
            }
        }
        self.sync(cx);
    }

    fn set_page(&mut self, cx: &mut Cx, page: Page) {
        self.model.page = page;
        self.sync(cx);
    }

    /// Puts the model's layout into the shell: which sidebar shows and which tab it
    /// marks, which detail panes and sections are folded.
    fn sync_shell(&mut self, cx: &mut Cx) {
        let m = &self.model;
        let ui = &self.ui;
        let l = &m.layout;
        ui.view(cx, ids!(side_wide))
            .set_visible(cx, !l.sidebar_collapsed);
        ui.view(cx, ids!(side_narrow))
            .set_visible(cx, l.sidebar_collapsed);
        let narrow = [
            (Page::Agents, ids!(ntab_agents)),
            (Page::Sessions, ids!(ntab_sessions)),
            (Page::Friction, ids!(ntab_friction)),
            (Page::History, ids!(ntab_history)),
            (Page::Data, ids!(ntab_data)),
        ];
        for (page, tab) in narrow {
            let tab = ui.widget(cx, tab);
            set_bg(cx, &tab, if m.page == page { 0x3b82f6a0 } else { CLEAR });
        }
        for id in [ids!(auto), ids!(nauto)] {
            let cb = ui.check_box(cx, id);
            if cb.active(cx) != m.auto_refresh {
                cb.set_active(cx, m.auto_refresh, Animate::No);
            }
        }
        for (page, split, strip, _, _) in details() {
            let open = l.detail_open(page);
            let s = ui.splitter(cx, split);
            let want = if open {
                SplitterCollapse::None
            } else {
                SplitterCollapse::B
            };
            if s.collapse() != want {
                s.set_collapse(cx, want);
            }
            ui.view(cx, strip).set_visible(cx, !open);
        }
        for (section, head, body) in sections() {
            let open = l.section_open(section);
            ui.button(cx, head).set_text(cx, &section.header(open));
            ui.view(cx, body).set_visible(cx, open);
        }
    }

    /// Puts each list's order into its headers and sort bar.
    fn sync_sort(&mut self, cx: &mut Cx) {
        for (n, (list, id)) in sort_lists().into_iter().enumerate() {
            let w = self.ui.widget(cx, id);
            let order = self.model.order(list);
            for (i, h) in head_ids().iter().enumerate().take(order.columns()) {
                let head = w.widget(cx, h).label(cx, ids!(head));
                head.set_text(cx, &order.header(i));
                set_color(cx, head, if order.sorts_by(i) { BUSY } else { META });
            }
            w.label(cx, ids!(sort_words)).set_text(cx, &order.words());
            w.button(cx, ids!(sort_reset))
                .enable(cx, !order.is_default());
            let labels = remove_labels(&self.model, list);
            if labels != self.sort_remove_labels[n] {
                let dd = w.drop_down(cx, ids!(sort_remove));
                dd.set_labels(cx, labels.clone());
                dd.set_selected_item(cx, 0);
                self.sort_remove_labels[n] = labels;
            }
        }
    }

    /// A header click, a removal or a reset in a list's sort bar.
    fn sort_actions(&mut self, cx: &mut Cx, actions: &Actions) {
        for (list, id) in sort_lists() {
            let w = self.ui.widget(cx, id);
            let columns = self.model.order(list).columns();
            for (i, h) in head_ids().iter().enumerate().take(columns) {
                if let Some(e) = w.view(cx, h).finger_up(actions)
                    && e.is_over
                {
                    let add = e.modifiers.shift || w.check_box(cx, ids!(sort_multi)).active(cx);
                    self.model.sort_click(list, i, add);
                    self.sync(cx);
                }
            }
            if let Some(i) = w.drop_down(cx, ids!(sort_remove)).changed(actions)
                && i > 0
            {
                self.model.sort_remove(list, i - 1);
                self.sync(cx);
            }
            if w.button(cx, ids!(sort_reset)).clicked(actions) {
                self.model.sort_reset(list);
                self.sync(cx);
            }
        }
    }

    /// Puts the model's state into everything that isn't a list.
    fn sync(&mut self, cx: &mut Cx) {
        self.sync_shell(cx);
        self.sync_sort(cx);
        let m = &self.model;
        let ui = &self.ui;
        let now = Utc::now();

        let pages = [
            (Page::Agents, ids!(agents_page), ids!(tab_agents)),
            (Page::Sessions, ids!(sessions_page), ids!(tab_sessions)),
            (Page::Friction, ids!(friction_page), ids!(tab_friction)),
            (Page::History, ids!(history_page), ids!(tab_history)),
            (Page::Data, ids!(data_page), ids!(tab_data)),
        ];
        for (page, view, tab) in pages {
            ui.view(cx, view).set_visible(cx, m.page == page);
            let on = m.page == page;
            let tab = ui.widget(cx, tab);
            set_bg(cx, &tab, if on { 0x3b82f6a0 } else { CLEAR });
        }
        ui.label(cx, ids!(status)).set_text(cx, &m.status_line(now));
        let src = match &self.source {
            Some(s) if s.fixture.is_some() => {
                let name = s
                    .fixture
                    .as_ref()
                    .and_then(|f| f.file_name())
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if s.agent.is_some() {
                    format!("fixture {name} · actions run AGENT_MANAGER_AGENT")
                } else {
                    format!("fixture {name} · actions not run")
                }
            }
            _ => String::new(),
        };
        let src = match runner::live_note() {
            Some(live) if src.is_empty() => live,
            Some(live) => format!("{live} · {src}"),
            None => src,
        };
        ui.label(cx, ids!(source)).set_text(cx, &src);

        // Agents.
        let offers = m.agent_offers();
        let set_offer = |cx: &mut Cx, id: &[LiveId], offer: Option<&Offer>| {
            let ready = offer.is_some_and(|o| o.command().is_some());
            ui.button(cx, id).enable(cx, ready);
        };
        set_offer(cx, ids!(resume), offers.as_ref().map(|o| &o.resume));
        set_offer(cx, ids!(revive), offers.as_ref().map(|o| &o.revive));
        set_offer(cx, ids!(cleanup), offers.as_ref().map(|o| &o.cleanup));
        match (m.agent(), &offers) {
            (Some((h, a)), Some(o)) => {
                let agent = &m.view.hosts[h].agents[a];
                ui.label(cx, ids!(agent_title))
                    .set_text(cx, &format!("{}  on {}", agent.name, m.view.hosts[h].host));
                let mut why = Vec::new();
                for (name, offer) in [
                    ("resume", &o.resume),
                    ("revive", &o.revive),
                    ("clean up", &o.cleanup),
                ] {
                    if let Some(w) = offer.why_off() {
                        why.push(format!("{name}: {w}"));
                    }
                }
                ui.label(cx, ids!(agent_why)).set_text(cx, &why.join("\n"));
                // An empty label still takes a line.
                ui.widget(cx, ids!(agent_why))
                    .set_visible(cx, !why.is_empty());
                ui.label(cx, ids!(agent_detail))
                    .set_text(cx, &agents::detail(&m.view, h, a));
                let logs = match &m.agent_logs {
                    Some((k, text)) if Some(k) == m.selected_agent.as_ref() => text.clone(),
                    _ => "reading…".to_string(),
                };
                ui.label(cx, ids!(agent_logs)).set_text(cx, &logs);
            }
            _ => {
                ui.label(cx, ids!(agent_title))
                    .set_text(cx, "Select an agent");
                ui.label(cx, ids!(agent_why)).set_text(cx, "");
                ui.label(cx, ids!(agent_detail)).set_text(cx, "");
                ui.label(cx, ids!(agent_logs)).set_text(cx, "");
            }
        }

        // Sessions.
        let mut hosts = vec!["all hosts".to_string()];
        hosts.extend(m.host_names());
        if hosts != self.host_labels {
            ui.drop_down(cx, ids!(host_dd))
                .set_labels(cx, hosts.clone());
            let sel = m
                .filter
                .host
                .as_ref()
                .and_then(|h| hosts.iter().position(|x| x == h))
                .unwrap_or(0);
            ui.drop_down(cx, ids!(host_dd)).set_selected_item(cx, sel);
            self.host_labels = hosts;
        }
        let mut harnesses = vec!["all harnesses".to_string()];
        harnesses.extend(sessions::harnesses(&m.view));
        if harnesses != self.harness_labels {
            ui.drop_down(cx, ids!(harness_dd))
                .set_labels(cx, harnesses.clone());
            let sel = m
                .filter
                .harness
                .as_ref()
                .and_then(|h| harnesses.iter().position(|x| x == h))
                .unwrap_or(0);
            ui.drop_down(cx, ids!(harness_dd))
                .set_selected_item(cx, sel);
            self.harness_labels = harnesses;
        }
        ui.label(cx, ids!(totals)).set_text(cx, &m.totals.text());
        let n_sel = m.selected_sessions.len();
        ui.label(cx, ids!(selected)).set_text(
            cx,
            &if n_sel > 0 {
                format!("{n_sel} selected")
            } else {
                String::new()
            },
        );
        let selected = m.selected_lines();
        ui.button(cx, ids!(untrack))
            .enable(cx, selected.iter().any(|l| !l.row(&m.view).untracked));
        ui.button(cx, ids!(track)).enable(
            cx,
            agent_manager_core::commands::track(&m.view, &selected)
                .command()
                .is_some(),
        );
        ui.button(cx, ids!(clear_sel)).enable(cx, n_sel > 0);
        let nr = m.not_running_count();
        let all = ui.button(cx, ids!(untrack_all));
        all.set_text(cx, &format!("Stop tracking all {nr} shown not running…"));
        all.enable(cx, nr > 0);
        let (title, detail, tokens) = match m.focused_line() {
            Some(l) => {
                let s = l.row(&m.view);
                let title = if s.title.is_empty() {
                    s.key()
                } else {
                    s.title.clone()
                };
                (
                    title,
                    sessions::detail(&m.view, &l),
                    sessions::tokens(&m.view, &l),
                )
            }
            None => (
                "Session".to_string(),
                "Click a session to select it (and see its detail here); click again to unselect."
                    .to_string(),
                String::new(),
            ),
        };
        ui.label(cx, ids!(session_title)).set_text(cx, &title);
        ui.label(cx, ids!(session_detail)).set_text(cx, &detail);
        ui.label(cx, ids!(session_tokens)).set_text(cx, &tokens);

        // Friction.
        match m.friction() {
            Some((h, i)) => {
                let f = &m.view.hosts[h].friction[i];
                ui.label(cx, ids!(friction_title))
                    .set_text(cx, &format!("#{} on {}", f.id, m.view.hosts[h].host));
                ui.label(cx, ids!(friction_detail))
                    .set_text(cx, &friction::detail(&m.view, h, i, now));
            }
            None => {
                ui.label(cx, ids!(friction_title))
                    .set_text(cx, "Select an entry");
                ui.label(cx, ids!(friction_detail)).set_text(cx, "");
            }
        }
        let note = ui.text_input(cx, ids!(friction_note)).text();
        let offer = m.resolve_offer(&note);
        ui.button(cx, ids!(resolve))
            .enable(cx, offer.command().is_some());
        ui.label(cx, ids!(friction_why))
            .set_text(cx, offer.why_off().unwrap_or(""));

        // History.
        let total: usize = m.view.hosts.iter().map(|h| h.history.len()).sum();
        ui.label(cx, ids!(history_count)).set_text(
            cx,
            &format!("{} of {total} commands", m.history_lines.len()),
        );
        let (title, detail) = match m.history_line() {
            Some(l) => {
                let c = history::cells(&m.view, &l, now);
                (
                    format!("{} on {}, exit {}", c.who, c.host, c.exit),
                    history::detail(&m.view, &l, now),
                )
            }
            None => (
                "Command".to_string(),
                "Click a command to see the whole of it here.".to_string(),
            ),
        };
        ui.label(cx, ids!(history_title)).set_text(cx, &title);
        ui.label(cx, ids!(history_detail)).set_text(cx, &detail);

        // Data: the list draws its lines from the model; this sets the buttons.
        ui.widget(cx, ids!(data_list)).redraw(cx);
        let backup = agent_manager_core::commands::backup(&m.view);
        let busy = m.running.is_some();
        ui.button(cx, ids!(backup))
            .enable(cx, backup.command().is_some() && !busy);
        ui.button(cx, ids!(prune))
            .enable(cx, backup.command().is_some() && !busy);
        let why = match (&backup, data::parse_days(&m.prune_days)) {
            (Offer::Off(w), _) => w.clone(),
            (_, Err(e)) => e,
            _ => format!(
                "Prune runs a dry run first and shows what it would delete; data of {} only.",
                m.view
                    .hosts
                    .get(agent_manager_core::local_index(&m.view))
                    .map(|h| h.host.as_str())
                    .unwrap_or("this host")
            ),
        };
        ui.label(cx, ids!(data_why)).set_text(cx, &why);

        // The dialog.
        let modal = ui.modal(cx, ids!(dialog));
        match &m.dialog {
            Some(d) => {
                let (title, body, yes) = match d {
                    Dialog::Confirm(c) => (
                        c.title.clone(),
                        format!("{}\nRuns: {}\n", c.body, c.command.line()),
                        Some(c.yes.clone()),
                    ),
                    Dialog::Result(o) => (
                        o.headline(),
                        format!("$ {}\n\n{}", o.command.line(), o.text()),
                        None,
                    ),
                };
                ui.label(cx, ids!(dialog_title)).set_text(cx, &title);
                ui.label(cx, ids!(dialog_body)).set_text(cx, &body);
                let yes_b = ui.button(cx, ids!(dialog_yes));
                yes_b.set_visible(cx, yes.is_some());
                if let Some(y) = yes {
                    yes_b.set_text(cx, &y);
                }
                ui.button(cx, ids!(dialog_no)).set_text(
                    cx,
                    if matches!(d, Dialog::Confirm(_)) {
                        "Cancel"
                    } else {
                        "Close"
                    },
                );
                if !modal.is_open() {
                    modal.open(cx);
                }
            }
            None => {
                if modal.is_open() {
                    modal.close(cx);
                }
            }
        }
        ui.redraw(cx);
    }
}

impl App {
    /// The sidebar's narrow form, and folding and unfolding detail panes and sections.
    fn shell_actions(&mut self, cx: &mut Cx, actions: &Actions) {
        let ui = self.ui.clone();
        let mut changed = false;
        if ui.button(cx, ids!(side_fold)).clicked(actions)
            || ui.button(cx, ids!(side_unfold)).clicked(actions)
        {
            self.model.layout.toggle_sidebar();
            changed = true;
        }
        let narrow = [
            (ids!(ntab_agents), Page::Agents),
            (ids!(ntab_sessions), Page::Sessions),
            (ids!(ntab_friction), Page::Friction),
            (ids!(ntab_history), Page::History),
            (ids!(ntab_data), Page::Data),
        ];
        for (id, page) in narrow {
            if ui.button(cx, id).clicked(actions) {
                self.model.page = page;
                changed = true;
            }
        }
        if ui.button(cx, ids!(nrefresh)).clicked(actions) {
            self.refresh(cx);
        }
        if let Some(on) = ui.check_box(cx, ids!(nauto)).changed(actions) {
            self.model.auto_refresh = on;
            changed = true;
        }
        for (page, split, _, hide, show) in details() {
            if ui.button(cx, hide).clicked(actions) {
                self.model.layout.set_detail_open(page, false);
                changed = true;
            }
            if ui.button(cx, show).clicked(actions) {
                self.model.layout.set_detail_open(page, true);
                changed = true;
            }
            // Folded or brought back by a hand on the bar.
            if let Some(c) = ui.splitter(cx, split).collapsed(actions) {
                let open = c == SplitterCollapse::None;
                if self.model.layout.detail_open(page) != open {
                    self.model.layout.set_detail_open(page, open);
                    changed = true;
                }
            }
        }
        for (section, head, _) in sections() {
            if ui.button(cx, head).clicked(actions) {
                self.model.layout.toggle_section(section);
                changed = true;
            }
        }
        if changed {
            self.sync(cx);
        }
    }
}

impl MatchEvent for App {
    fn handle_startup(&mut self, cx: &mut Cx) {
        self.model = Model::new();
        self.source = Some(Source::from_env());
        self.tick = cx.start_interval(1.0);
        self.refresh(cx);
    }

    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions) {
        // Results from background jobs.
        for action in actions {
            match action.downcast_ref::<Msg>() {
                Some(Msg::View(Ok(v))) => {
                    self.model.set_view(v.clone(), Utc::now());
                    self.sync(cx);
                }
                Some(Msg::View(Err(e))) => {
                    self.model.set_load_error(e.clone());
                    self.sync(cx);
                }
                Some(Msg::Done(job, out)) => {
                    let effect = self.model.finished(job.clone(), out.clone());
                    self.apply(cx, effect);
                }
                None => {}
            }
        }

        let ui = self.ui.clone();
        let tabs = [
            (ids!(tab_agents), Page::Agents),
            (ids!(tab_sessions), Page::Sessions),
            (ids!(tab_friction), Page::Friction),
            (ids!(tab_history), Page::History),
            (ids!(tab_data), Page::Data),
        ];
        for (id, page) in tabs {
            if ui.button(cx, id).clicked(actions) {
                self.set_page(cx, page);
            }
        }
        if ui.button(cx, ids!(refresh)).clicked(actions) {
            self.refresh(cx);
        }
        if let Some(on) = ui.check_box(cx, ids!(auto)).changed(actions) {
            self.model.auto_refresh = on;
        }
        self.shell_actions(cx, actions);
        self.sort_actions(cx, actions);

        // Agents.
        let list = ui.widget(cx, ids!(agent_list)).portal_list(cx, ids!(list));
        for (i, item) in list.items_with_actions(actions) {
            if item
                .view(cx, ids!(row))
                .finger_up(actions)
                .is_some_and(|e| e.is_over)
            {
                let effect = self.model.select_agent_line(i);
                self.apply(cx, effect);
            }
        }
        if ui.button(cx, ids!(resume)).clicked(actions) {
            let effect = self.model.resume();
            self.apply(cx, effect);
        }
        if ui.button(cx, ids!(revive)).clicked(actions) {
            let effect = self.model.revive();
            self.apply(cx, effect);
        }
        if ui.button(cx, ids!(cleanup)).clicked(actions) {
            let effect = self.model.cleanup();
            self.apply(cx, effect);
        }

        // Sessions.
        if let Some(text) = ui.text_input(cx, ids!(filter_text)).changed(actions) {
            self.model.set_filter_text(&text);
            self.sync(cx);
        }
        let mut refilter = false;
        if let Some(i) = ui.drop_down(cx, ids!(host_dd)).changed(actions) {
            self.model.filter.host = (i > 0).then(|| self.host_labels[i].clone());
            refilter = true;
        }
        if let Some(i) = ui.drop_down(cx, ids!(harness_dd)).changed(actions) {
            self.model.filter.harness = (i > 0).then(|| self.harness_labels[i].clone());
            refilter = true;
        }
        if let Some(i) = ui.drop_down(cx, ids!(state_dd)).changed(actions) {
            self.model.filter.state = sessions::state_choices().get(i).and_then(|c| c.1);
            refilter = true;
        }
        if let Some(on) = ui.check_box(cx, ids!(stale_cb)).changed(actions) {
            self.model.filter.stale_only = on;
            refilter = true;
        }
        if let Some(on) = ui.check_box(cx, ids!(untracked_cb)).changed(actions) {
            self.model.filter.show_untracked = on;
            refilter = true;
        }
        if refilter {
            self.model.refilter();
            self.sync(cx);
        }
        let list = ui
            .widget(cx, ids!(session_list))
            .portal_list(cx, ids!(list));
        for (i, item) in list.items_with_actions(actions) {
            let checked = item.check_box(cx, ids!(check)).changed(actions);
            let tapped = item
                .view(cx, ids!(row))
                .finger_up(actions)
                .is_some_and(|e| e.is_over);
            if let Some(on) = checked {
                if let Some(l) = self.model.session_lines.get(i).copied()
                    && self.model.is_selected(&l) != on
                {
                    self.model.toggle_session_line(i);
                }
                self.sync(cx);
            } else if tapped {
                self.model.toggle_session_line(i);
                self.sync(cx);
            }
        }
        if ui.button(cx, ids!(select_all)).clicked(actions) {
            self.model.select_all_shown();
            self.sync(cx);
        }
        if ui.button(cx, ids!(clear_sel)).clicked(actions) {
            self.model.clear_selection();
            self.sync(cx);
        }
        if ui.button(cx, ids!(untrack)).clicked(actions) {
            let effect = self.model.untrack_selected();
            self.apply(cx, effect);
        }
        if ui.button(cx, ids!(untrack_all)).clicked(actions) {
            let effect = self.model.untrack_not_running();
            self.apply(cx, effect);
        }
        if ui.button(cx, ids!(track)).clicked(actions) {
            let effect = self.model.track_selected();
            self.apply(cx, effect);
        }

        // Friction.
        let list = ui
            .widget(cx, ids!(friction_list))
            .portal_list(cx, ids!(list));
        for (i, item) in list.items_with_actions(actions) {
            if item
                .view(cx, ids!(row))
                .finger_up(actions)
                .is_some_and(|e| e.is_over)
            {
                self.model.select_friction_line(i);
                self.sync(cx);
            }
        }
        if ui
            .text_input(cx, ids!(friction_note))
            .changed(actions)
            .is_some()
        {
            self.sync(cx);
        }
        if ui.button(cx, ids!(resolve)).clicked(actions) {
            let note = ui.text_input(cx, ids!(friction_note)).text();
            let effect = self.model.resolve(&note);
            self.apply(cx, effect);
        }

        // History.
        if let Some(text) = ui.text_input(cx, ids!(history_text)).changed(actions) {
            self.model.set_history_text(&text);
            self.sync(cx);
        }
        if let Some(on) = ui.check_box(cx, ids!(failed_cb)).changed(actions) {
            self.model.set_history_failed_only(on);
            self.sync(cx);
        }
        let list = ui
            .widget(cx, ids!(history_list))
            .portal_list(cx, ids!(list));
        for (i, item) in list.items_with_actions(actions) {
            if item
                .view(cx, ids!(row))
                .finger_up(actions)
                .is_some_and(|e| e.is_over)
            {
                self.model.select_history_line(i);
                self.sync(cx);
            }
        }

        // Data.
        if let Some(text) = ui.text_input(cx, ids!(prune_days)).changed(actions) {
            self.model.prune_days = text;
            self.sync(cx);
        }
        if ui.button(cx, ids!(backup)).clicked(actions) {
            let effect = self.model.backup();
            self.apply(cx, effect);
        }
        if ui.button(cx, ids!(prune)).clicked(actions) {
            let effect = self.model.prune();
            self.apply(cx, effect);
        }

        // The dialog.
        if ui.button(cx, ids!(dialog_yes)).clicked(actions) {
            let effect = self.model.confirmed();
            self.apply(cx, effect);
        }
        if ui.button(cx, ids!(dialog_no)).clicked(actions) {
            self.model.dismiss();
            self.sync(cx);
        }
        if ui.modal(cx, ids!(dialog)).dismissed(actions) {
            self.model.dismiss();
            self.sync(cx);
        }
    }
}

impl AppMain for App {
    fn script_mod(vm: &mut ScriptVm) -> ScriptValue {
        crate::makepad_widgets::script_mod(vm);
        self::script_mod(vm)
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event) {
        if self.tick.is_event(event).is_some() {
            let due = self
                .last_read
                .is_none_or(|t| t.elapsed().as_secs_f64() >= AUTO_REFRESH_SECS);
            if self.model.auto_refresh && due {
                self.refresh(cx);
            } else {
                let now = Utc::now();
                self.ui
                    .label(cx, ids!(status))
                    .set_text(cx, &self.model.status_line(now));
            }
        }
        self.match_event(cx, event);
        self.ui
            .handle_event(cx, event, &mut Scope::with_data(&mut self.model));
        // A hot reload (`--hot`) rebuilt the widgets from the script: everything this
        // layer sets on them is gone, so put the model's state back.
        if let Event::LiveEdit = event {
            self.sort_remove_labels = Default::default();
            self.sync(cx);
        }
    }
}
