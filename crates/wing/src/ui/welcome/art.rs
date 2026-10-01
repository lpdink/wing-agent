//! 海鸥帧数据 —— 创作期光栅器（矢量形状 → 硬边量化 → 外部描边）的定稿导出。
//!
//! 数据形态与 dsh 的像素鲸鱼同构：字母网格，`.` 透明，字母即调色板键
//!（见 [`super::sprite::brand`]）。运行期零生成成本；改画只需要改字符串。
//! 两个姿态：[`PERCHED_*`] 待机（站姿 chibi）、[`FLY_*`] 干活（飞行扇翅）。

/// 待机：站姿 chibi 的标准姿势。
///
/// 23 像素行 × 29 列（终端行数是布局的事，见 `super`）。
pub const PERCHED_IDLE: &[&str] = &[
    ".................OOOO........",
    "...............OOWWWWOO......",
    "..............OWWWWWWWWO.....",
    ".............OWWWWWWWWWWO....",
    ".............OWWWWWWWWWWO....",
    "............OWWWWWWWHEWWWO...",
    ".......OOOOOOWWWWWWWEEWWBOOO.",
    "......OGGGGWWWWWWWWWWWWWBBBBO",
    "......OGGGGGGWWWWWWWWWWWMMBO.",
    "......OWWGGGGGWWWWWWWWWWOOO..",
    ".....OWWWWGGGGGGGWWWWWWWO....",
    ".....OWWWWGGGGGGGGWWWWWWO....",
    "....OWWWWGGGGGGGGGGWWWWWWO...",
    "....OWWWWDDDGGGGSGWWWWWWWO...",
    "...OOOWWWDDDSSSSSWWWWWWWO....",
    "..OWWWWWWDDGGSSSWWWWWWWWO....",
    ".ODDDDWWWWWWWWWWWWWWWWWO.....",
    "..ODDDWWWWWWWWWWWWWWWWO......",
    "...OOOOOOOWWWWWWWWWWOO.......",
    "..........OOMMWWMMOO.........",
    "...........OMM..MMO..........",
    "..........OMMMMMMMMO.........",
    "...........OOOOOOOO..........",
];

/// [`PERCHED_IDLE`] 的列数。
pub const PERCHED_IDLE_COLS: usize = 29;
/// [`PERCHED_IDLE`] 的像素行数。
pub const PERCHED_IDLE_ROWS: usize = 23;
/// 待机：闭眼帧。
///
/// 23 像素行 × 29 列（终端行数是布局的事，见 `super`）。
pub const PERCHED_BLINK: &[&str] = &[
    ".................OOOO........",
    "...............OOWWWWOO......",
    "..............OWWWWWWWWO.....",
    ".............OWWWWWWWWWWO....",
    ".............OWWWWWWWWWWO....",
    "............OWWWWWWWWWWWWO...",
    ".......OOOOOOWWWWWWEEEEWBOOO.",
    "......OGGGGWWWWWWWWWWWWWBBBBO",
    "......OGGGGGGWWWWWWWWWWWMMBO.",
    "......OWWGGGGGWWWWWWWWWWOOO..",
    ".....OWWWWGGGGGGGWWWWWWWO....",
    ".....OWWWWGGGGGGGGWWWWWWO....",
    "....OWWWWGGGGGGGGGGWWWWWWO...",
    "....OWWWWDDDGGGGSGWWWWWWWO...",
    "...OOOWWWDDDSSSSSWWWWWWWO....",
    "..OWWWWWWDDGGSSSWWWWWWWWO....",
    ".ODDDDWWWWWWWWWWWWWWWWWO.....",
    "..ODDDWWWWWWWWWWWWWWWWO......",
    "...OOOOOOOWWWWWWWWWWOO.......",
    "..........OOMMWWMMOO.........",
    "...........OMM..MMO..........",
    "..........OMMMMMMMMO.........",
    "...........OOOOOOOO..........",
];

/// [`PERCHED_BLINK`] 的列数。
pub const PERCHED_BLINK_COLS: usize = 29;
/// [`PERCHED_BLINK`] 的像素行数。
pub const PERCHED_BLINK_ROWS: usize = 23;
/// 待机：抖翅第 1 帧。
///
/// 23 像素行 × 29 列（终端行数是布局的事，见 `super`）。
pub const PERCHED_FLUTTER1: &[&str] = &[
    ".................OOOO........",
    "...............OOWWWWOO......",
    "..............OWWWWWWWWO.....",
    ".............OWWWWWWWWWWO....",
    ".............OWWWWWWWWWWO....",
    "............OWWWWWWWHEWWWO...",
    ".......OOOOOOWWWWWWWEEWWBOOO.",
    "......OGGGGWWWWWWWWWWWWWBBBBO",
    "......OGGGGGGWWWWWWWWWWWMMBO.",
    "......OWWGGGGGWWWWWWWWWWOOO..",
    ".....OWWWWWGGGWWWWWWWWWWO....",
    ".....OWWWWGGGGGGGGWWWWWWO....",
    "....OWWWWWGGGGGGGGWWWWWWWO...",
    "....OWWWWGGGGGGGGGGWWWWWWO...",
    "...OOOWWDDDDSSSSSGWWWWWWO....",
    "..OWWWWWWDDDSSSSWWWWWWWWO....",
    ".ODDDDWWWWWWGGWWWWWWWWWO.....",
    "..ODDDWWWWWWWWWWWWWWWWO......",
    "...OOOOOOOWWWWWWWWWWOO.......",
    "..........OOMMWWMMOO.........",
    "...........OMM..MMO..........",
    "..........OMMMMMMMMO.........",
    "...........OOOOOOOO..........",
];

/// [`PERCHED_FLUTTER1`] 的列数。
pub const PERCHED_FLUTTER1_COLS: usize = 29;
/// [`PERCHED_FLUTTER1`] 的像素行数。
pub const PERCHED_FLUTTER1_ROWS: usize = 23;
/// 待机：抖翅第 2 帧。
///
/// 23 像素行 × 29 列（终端行数是布局的事，见 `super`）。
pub const PERCHED_FLUTTER2: &[&str] = &[
    ".................OOOO........",
    "...............OOWWWWOO......",
    "..............OWWWWWWWWO.....",
    ".............OWWWWWWWWWWO....",
    ".............OWWWWWWWWWWO....",
    "............OWWWWWWWHEWWWO...",
    ".......OOOOOOWWWWWWWEEWWBOOO.",
    "......OGGGGWWWWWWWWWWWWWBBBBO",
    "......OGGGGGGWWWWWWWWWWWMMBO.",
    "......OWWGGGGGWWWWWWWWWWOOO..",
    ".....OWWWWWGGGWWWWWWWWWWO....",
    ".....OWWWWWGGGGGGWWWWWWWO....",
    "....OWWWWWGGGGGGGGWWWWWWWO...",
    "....OWWWWGGGGGGGGGGWWWWWWO...",
    "...OOOWWWDDGGGGGGGGWWWWWO....",
    "..OWWWWWWDDDSSSSSWWWWWWWO....",
    ".ODDDDWWWDDDSSSSWWWWWWWO.....",
    "..ODDDWWWWWWWWWWWWWWWWO......",
    "...OOOOOOOWWWWWWWWWWOO.......",
    "..........OOMMWWMMOO.........",
    "...........OMM..MMO..........",
    "..........OMMMMMMMMO.........",
    "...........OOOOOOOO..........",
];

/// [`PERCHED_FLUTTER2`] 的列数。
pub const PERCHED_FLUTTER2_COLS: usize = 29;
/// [`PERCHED_FLUTTER2`] 的像素行数。
pub const PERCHED_FLUTTER2_ROWS: usize = 23;
/// 干活：扇翅循环第 0 帧（共 6，循环播放）。
///
/// 21 像素行 × 37 列（终端行数是布局的事，见 `super`）。
pub const FLY_0: &[&str] = &[
    "...........OO........................",
    "..........ODDOO......................",
    ".........ODDDDDO.....................",
    ".........ODDDDDWO....................",
    "..........ODDDWWWO...................",
    "..........ODDSWWWWO.........OO.......",
    "...........OWSSWWWWO......OOWWOO.....",
    "...........OWWSSWWWWO....OWWWWWWO....",
    "............OWSSSWWWWO..OWWWWEEWWO...",
    "............OWWSSWWWWWOOWWWWWEEWWOOO.",
    ".............OWWSSWWWWWWWWWWWEEWBBBBO",
    ".........OOOOGWWSSSWWWWWWWWWWWWWBMBBO",
    "........OGGGGGGWWSSSWWWWWWWWWWWWWMOO.",
    "........OGGGGGGWWWSSSWWWWWWWWWWWWO...",
    ".....OOOWWWWWWWWWWSSSWWWWWWWWWWWO....",
    "...OOWWWWWWWWWWWWWWSSWWWWWWWWWOO.....",
    "..ODDDWWWWWWWWGGGGGGGGGGWWWOOO.......",
    "..ODDDWWWWWGGGGGGGGGGGGGWWO..........",
    "..ODDDOOOOGGGGGGGGGGGWWWOO...........",
    "...OOO....OOOWWWWWWWOOOO.............",
    ".............OOOOOOO.................",
];

/// [`FLY_0`] 的列数。
pub const FLY_0_COLS: usize = 37;
/// [`FLY_0`] 的像素行数。
pub const FLY_0_ROWS: usize = 21;
/// 干活：扇翅循环第 1 帧（共 6，循环播放）。
///
/// 20 像素行 × 37 列（终端行数是布局的事，见 `super`）。
pub const FLY_1: &[&str] = &[
    "...........O.........................",
    "..........ODOO.......................",
    ".........ODDDWO......................",
    "........ODDDDDWOO....................",
    ".........ODDDDWWWO..........OO.......",
    ".........ODDDWWWWWO.......OOWWOO.....",
    "..........OWSSWWWWWOO....OWWWWWWO....",
    "...........OWSSWWWWWWO..OWWWWEEWWO...",
    "...........OWWSSWWWWWWOOWWWWWEEWWOOO.",
    "............OWWSSWWWWWWWWWWWWEEWBBBBO",
    ".........OOOOGWSSSWWWWWWWWWWWWWWBMBBO",
    "........OGGGGGWWSSSWWWWWWWWWWWWWWMOO.",
    "........OGGGGGGWWSSSWWWWWWWWWWWWWO...",
    ".....OOOWWWWWWWWWWSSSWWWWWWWWWWWO....",
    "...OOWWWWWWWWWWWWWWSSWWWWWWWWWOO.....",
    "..ODDDWWWWWWWWGGGGGGGGGGWWWOOO.......",
    "..ODDDWWWWWGGGGGGGGGGGGGWWO..........",
    "..ODDDOOOOGGGGGGGGGGGWWWOO...........",
    "...OOO....OOOWWWWWWWOOOO.............",
    ".............OOOOOOO.................",
];

/// [`FLY_1`] 的列数。
pub const FLY_1_COLS: usize = 37;
/// [`FLY_1`] 的像素行数。
pub const FLY_1_ROWS: usize = 20;
/// 干活：扇翅循环第 2 帧（共 6，循环播放）。
///
/// 18 像素行 × 37 列（终端行数是布局的事，见 `super`）。
pub const FLY_2: &[&str] = &[
    "..........OOO........................",
    ".........ODWWOO......................",
    "........ODDDWWWOO...........OO.......",
    ".......ODDDDDWWWWO........OOWWOO.....",
    "........ODDDDWWWWWOO.....OWWWWWWO....",
    ".........ODSSSWWWWWWOO..OWWWWEEWWO...",
    "..........OWSSSWWWWWWWOOWWWWWEEWWOOO.",
    "...........OWSSSWWWWWWWWWWWWWEEWBBBBO",
    ".........OOOOWSSSSWWWWWWWWWWWWWWBMBBO",
    "........OGGGGGWSSSSWWWWWWWWWWWWWWMOO.",
    "........OGGGGGGWSSSSWWWWWWWWWWWWWO...",
    ".....OOOWWWWWWWWWWSSSWWWWWWWWWWWO....",
    "...OOWWWWWWWWWWWWWWSSWWWWWWWWWOO.....",
    "..ODDDWWWWWWWWGGGGGGGGGGWWWOOO.......",
    "..ODDDWWWWWGGGGGGGGGGGGGWWO..........",
    "..ODDDOOOOGGGGGGGGGGGWWWOO...........",
    "...OOO....OOOWWWWWWWOOOO.............",
    ".............OOOOOOO.................",
];

/// [`FLY_2`] 的列数。
pub const FLY_2_COLS: usize = 37;
/// [`FLY_2`] 的像素行数。
pub const FLY_2_ROWS: usize = 18;
/// 干活：扇翅循环第 3 帧（共 6，循环播放）。
///
/// 17 像素行 × 37 列（终端行数是布局的事，见 `super`）。
pub const FLY_3: &[&str] = &[
    ".........OO..........................",
    "........OWWOOO..............OO.......",
    ".......ODDDWWWOOO.........OOWWOO.....",
    ".......ODDDDWWWWWOO......OWWWWWWO....",
    ".......ODDDDDWWWWWWOOO..OWWWWEEWWO...",
    ".......ODDDSSWWWWWWWWWOOWWWWWEEWWOOO.",
    "........ODOSSSSWWWWWWWWWWWWWWEEWBBBBO",
    ".........OOOWSSSWWWWWWWWWWWWWWWWBMBBO",
    "........OGGGGWSSSSWWWWWWWWWWWWWWWMOO.",
    "........OGGGGGWSSSSSWWWWWWWWWWWWWO...",
    ".....OOOWWWWWWWWWSSSSWWWWWWWWWWWO....",
    "...OOWWWWWWWWWWWWWSSSWWWWWWWWWOO.....",
    "..ODDDWWWWWWWWGGGWGGGGGGWWWOOO.......",
    "..ODDDWWWWWGGGGGGGGGGGGGWWO..........",
    "..ODDDOOOOGGGGGGGGGGGWWWOO...........",
    "...OOO....OOOWWWWWWWOOOO.............",
    ".............OOOOOOO.................",
];

/// [`FLY_3`] 的列数。
pub const FLY_3_COLS: usize = 37;
/// [`FLY_3`] 的像素行数。
pub const FLY_3_ROWS: usize = 17;
/// 干活：扇翅循环第 4 帧（共 6，循环播放）。
///
/// 16 像素行 × 37 列（终端行数是布局的事，见 `super`）。
pub const FLY_4: &[&str] = &[
    "............................OO.......",
    "..........OOOOO...........OOWWOO.....",
    ".........OWWWWWOOO.......OWWWWWWO....",
    "........ODDDDWWWWWOOO...OWWWWEEWWO...",
    "........ODDDDDWWWWWWWOOOWWWWWEEWWOOO.",
    "........ODDDDDWWWWWWWWWWWWWWWEEWBBBBO",
    ".........ODDSSSWWWWWWWWWWWWWWWWWBMBBO",
    "........OGGGGSSSSSWWWWWWWWWWWWWWWMOO.",
    "........OGGGGGWSSSSSWWWWWWWWWWWWWO...",
    ".....OOOWWWWWWWWSSSSSWWWWWWWWWWWO....",
    "...OOWWWWWWWWWWWWWSSSWWWWWWWWWOO.....",
    "..ODDDWWWWWWWWGGGWGGGGGGWWWOOO.......",
    "..ODDDWWWWWGGGGGGGGGGGGGWWO..........",
    "..ODDDOOOOGGGGGGGGGGGWWWOO...........",
    "...OOO....OOOWWWWWWWOOOO.............",
    ".............OOOOOOO.................",
];

/// [`FLY_4`] 的列数。
pub const FLY_4_COLS: usize = 37;
/// [`FLY_4`] 的像素行数。
pub const FLY_4_ROWS: usize = 16;
/// 干活：扇翅循环第 5 帧（共 6，循环播放）。
///
/// 16 像素行 × 37 列（终端行数是布局的事，见 `super`）。
pub const FLY_5: &[&str] = &[
    "............................OO.......",
    "..........................OOWWOO.....",
    "............OOOO.........OWWWWWWO....",
    "...........OWWWWOOOOO...OWWWWEEWWO...",
    "..........OWDWWWWWWWWOOOWWWWWEEWWOOO.",
    ".........ODDDDWWWWWWWWWWWWWWWEEWBBBBO",
    ".........ODDDDDWWWWWWWWWWWWWWWWWBMBBO",
    "........OGGDDDSSWWWWWWWWWWWWWWWWWMOO.",
    "........OGGDDWSSSSSWWWWWWWWWWWWWWO...",
    ".....OOOWWWWWWWSSSSSSWWWWWWWWWWWO....",
    "...OOWWWWWWWWWWWWSSSSWWWWWWWWWOO.....",
    "..ODDDWWWWWWWWGGGWWGGGGGWWWOOO.......",
    "..ODDDWWWWWGGGGGGGGGGGGGWWO..........",
    "..ODDDOOOOGGGGGGGGGGGWWWOO...........",
    "...OOO....OOOWWWWWWWOOOO.............",
    ".............OOOOOOO.................",
];

/// [`FLY_5`] 的列数。
pub const FLY_5_COLS: usize = 37;
/// [`FLY_5`] 的像素行数。
pub const FLY_5_ROWS: usize = 16;
/// wordmark 像素字 `WING`：5 像素行 × 23 列（`#` = 墨迹）。
pub const WORDMARK: &[&str] = &[
    "#.....#.###.#...#..###.",
    "#.....#..#..##..#.#....",
    "#..#..#..#..#.#.#.#.##.",
    "#.#.#.#..#..#..##.#...#",
    ".#...#..###.#...#..###.",
];
/// [`WORDMARK`] 的列数。
pub const WORDMARK_COLS: usize = 23;
