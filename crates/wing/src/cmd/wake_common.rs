//! `wing watch` 与 `wing wake` 共用的三形态分发（创建 / 列表 / 取消）。
//!
//! 两个子命令是同一组端点（`POST /api/wake/create` · `GET /api/wake/list` ·
//! `POST /api/wake/cancel`）上的两种登记方式，输出形状也只有一处差别：Create 的
//! 请求体（会话型 vs 定时型）。形态判定与各自的拒绝文案留在两个子命令里——那是
//! 用户可见措辞，两边本就不同。

use anyhow::Result;
use wing_api_client::GatewayClient as GatewayApiClient;
use wing_api_client::models::{WakeCreateRequest, WakeInfo};

use super::common;
use super::wake::{WakeCancelOutput, WakeOutput, format_wake_detail, format_wakes};
use crate::config::AppConfig;
use crate::config::ThemePalette;

/// 本次调用是哪一种形态（三种形态的参数集合互斥）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WakeMode {
    Create,
    List,
    Cancel(i64),
}

pub(crate) enum WakeCall {
    Create(WakeCreateRequest),
    List { all: bool },
    Cancel(i64),
}

/// 按形态发请求并输出。
pub(crate) async fn dispatch(http: &GatewayApiClient, call: WakeCall, json: bool) -> Result<()> {
    match call {
        WakeCall::Cancel(id) => {
            let resp = http.cancel_wake(id).await?;
            if json {
                common::print_json_compact(&WakeCancelOutput {
                    id,
                    canceled: resp.canceled,
                });
            } else if resp.canceled {
                println!("canceled {id}");
            } else {
                println!("{id} was already finished (nothing to cancel)");
            }
        }
        WakeCall::List { all } => {
            let resp = http.list_wakes(all).await?;
            if json {
                common::print_json_compact(&resp.wakes);
            } else {
                print!("{}", render_wakes(&resp.wakes, all));
            }
        }
        WakeCall::Create(request) => {
            let resp = http.create_wake(&request).await?;
            if json {
                common::print_json_compact(&WakeOutput { wake: resp.wake });
            } else {
                print!("{}", format_wake_detail(&resp.wake));
            }
        }
    }
    Ok(())
}

/// 列表的人读形态：注入当前的调色板与输出环境（表格宽度按终端）。
fn render_wakes(wakes: &[WakeInfo], all: bool) -> String {
    let palette = ThemePalette::from_config(&AppConfig::load().colors);
    let out = common::TableOutput::detect();
    format_wakes(wakes, all, &palette, &out)
}
