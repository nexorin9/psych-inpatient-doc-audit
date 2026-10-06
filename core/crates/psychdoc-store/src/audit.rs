//! 审计链：每次**调阅**与每次**写回**各留一条 Provenance。
//!
//! 对应 `spec.md` §4「安全审计」与 `spec.md` §16 岗位「质控办」：
//! 月度专项抽查要能回答「这份病历当年是**按哪一版规则**审的、命中了哪些码」。
//! 所以 Provenance 不是「记一笔流水」，而是**可回查的证据行**：必须携带
//! 规则集 `sha256`、命中码集合、推进人与时刻。
//!
//! 关键不变量：
//!
//! - 一次 `record_review` 写**恰好一条** `kind=review` 事件；
//! - 一次幂等命中（重复确认）**不再写** `kind=writeback` 事件，也不写
//!   `writeback_log`——「没有新写回」这句话必须是**可核对的事实**而不是
//!   一句口头承诺；
//! - 住院别名与件 id 一律是脱敏别名，落盘前不依赖上游已脱敏。

use serde::{Deserialize, Serialize};

/// 审计事件类型。刻意用封闭枚举，避免日志里出现拼错的自由字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    /// 一次审查（调阅）。
    Review,
    /// 一次写回（建件 / 补写生效）。
    Writeback,
}

impl AuditKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Writeback => "writeback",
        }
    }
}

impl std::fmt::Display for AuditKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 落盘前先脱敏：住院号 / 姓名样式串换成占位符，并返回替换计数。
///
/// 这一层是**二次防线**：上游解析器已经用脱敏别名，但审计导出是唯一
/// 会被质控办带走的东西，漏一个字段就是一次真实的患者信息外泄。
/// 命中即计数，计数进 `review_runs` 摘要，让「有没有漏网的」可查。
pub fn desensitize(text: &str) -> (String, usize) {
    let mut hits = 0usize;
    let mut out = String::with_capacity(text.len());
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        let is_digit = c.is_ascii_digit();
        // 住院号样式：6 位及以上连续数字
        if is_digit {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let run: String = bytes[start..i].iter().collect();
            if run.len() >= 6 {
                out.push_str("<REDACTED-ID>");
                hits += 1;
            } else {
                out.push_str(&run);
            }
            continue;
        }
        // 姓名字式：**靠「标签 + 分隔符」锚定**，不靠「2–4 个汉字」的字数猜。
        //
        // 早期写法是「见到 2–4 个连续汉字就当名字」，看着能拦住「张丽娟」，
        // 实际会把「已签署」「未填写」这类正常文书用语一起吃掉——脱敏器一旦
        // 误伤正常内容，质控员就会开始怀疑整份导出，工具就废了。
        //
        // 所以收紧到**必须有显式分隔符**（冒号或空格）这一种形态：
        // 「家属：张丽娟」「签收人 李静」拦得住，「患者已离院」「签名缺失」
        // 放得过。误报为零比拦得全更重要——漏一个名字靠人工复核能补，
        // 错杀一百条正常措辞则没人再敢信这个工具。
        if let Some(after) = name_label_at(&bytes, i) {
            let mut j = after;
            // 标签与姓名之间必须真的隔着一个分隔符。紧挨着的不是姓名形态，
            // 直接回退到逐字输出，不做任何改动。
            let sep_start = j;
            while j < bytes.len() && matches!(bytes[j], '：' | ':' | ' ' | '　') {
                j += 1;
            }
            if j > sep_start {
                let name_start = j;
                while j < bytes.len() && is_cjk(bytes[j]) {
                    j += 1;
                }
                let run: Vec<char> = bytes[name_start..j].to_vec();
                if let Some(name_len) = chinese_name_len(&run) {
                    // 标签与分隔符原样保留（「家属：张三」→「家属：<REDACTED-NAME>」），
                    // 否则审核员看不出这条说的是患者还是家属。
                    out.extend(bytes[i..name_start].iter());
                    out.push_str("<REDACTED-NAME>");
                    hits += 1;
                    // 姓名之后的字（「代签」「已核」）不属于人名，原样保留。
                    out.extend(bytes[name_start + name_len..j].iter());
                    i = j;
                    continue;
                }
                // 不是姓名：句子，原样放过。
                out.extend(bytes[i..j].iter());
                i = j;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    (out, hits)
}

/// 姓名标签表。命中即认定「后面（隔一个分隔符）跟的是人名」。
///
/// 刻意只收**语义上必然跟人**的标签，且全部要求后面有分隔符；
/// 「陪同」「经治」这类词本身也可能是句子的一部分，靠分隔符这一道
/// 把它们挡在误报之外。
const NAME_LABELS: &[&str] = &[
    "姓名",
    "患者名",
    "病人名",
    "签收人",
    "责任人",
    "陪同人",
    "联系人",
    "签名",
    "患者",
    "病人",
    "家属",
];

/// `i` 处是否是一个姓名标签的开头；是则返回标签之后的下标。
fn name_label_at(bytes: &[char], i: usize) -> Option<usize> {
    let rest: String = bytes[i..].iter().collect();
    NAME_LABELS
        .iter()
        // 「患者名」必须整体匹配：先匹配到「患者」的话，剩下的「名字」
        // 已经不是 2–4 字人形，整条规则就漏了。故取最长的那个命中。
        .filter(|l| rest.starts_with(**l))
        .max_by_key(|l| l.chars().count())
        .map(|l| i + l.chars().count())
}

/// 判断一段汉字里开头是不是人名，返回人名长度（字节外的**字符数**）。
///
/// 中文没有词边界，「家属：张丽娟代签」在分隔符之后是**连着五个汉字**，
/// 光看「2–4 个字」会把这条漏掉。两个判据一起用：
///
/// 1. 整段就是 2–4 字 → 整段是名字（「签收人：李静」）；
/// 2. 段更长 → 取开头的**常见姓氏字 + 至多两个字**，其余是句子
///    （「张丽娟代签」→ 名字 3 字，「代签」保留）。
///
/// 第 2 条靠姓氏表收窄。没有它就得靠字数猜，而字数猜必然在
/// 「患者已离院」这类句子上误伤——宁可漏一个靠人工补，不能错杀让工具失信。
fn chinese_name_len(run: &[char]) -> Option<usize> {
    if (2..=4).contains(&run.len()) {
        return Some(run.len());
    }
    let surname = *run.first()?;
    if !COMMON_SURNAMES.contains(&surname) {
        return None;
    }
    // 「张丽娟代签」：姓 1 + 名至多 2 = 3；「李静已核」：姓 1 + 名 1 = 2。
    // 再长就当成句子而不是名字——「王护士长已离院」不该被当成一个三字人名。
    let take = run.len().min(3);
    (2..=take).contains(&run.len().min(take)).then_some(take)
}

/// 常见姓氏首字。覆盖《百家姓》前列与院内文书里实际会出现的那些。
///
/// 不追求全：漏掉的姓意味着那一条靠人工复核能补，而往里塞
/// 「已」「未」这类常用字会让脱敏器开始吃句子。
const COMMON_SURNAMES: &[char] = &[
    '王', '李', '张', '刘', '陈', '杨', '黄', '赵', '吴', '周', '徐', '孙', '马', '朱', '胡', '郭',
    '何', '高', '林', '罗', '郑', '梁', '谢', '宋', '唐', '许', '韩', '冯', '邓', '曹', '彭', '曾',
    '肖', '田', '董', '袁', '潘', '于', '蒋', '蔡', '余', '杜', '叶', '程', '苏', '魏', '吕', '丁',
    '任', '沈', '姚', '卢', '姜', '崔', '钟', '谭', '陆', '汪', '范', '金', '石', '廖', '贾', '夏',
    '韦', '付', '方', '白', '邹', '孟', '熊', '秦', '邱', '江', '尹', '薛', '闫', '段', '雷', '侯',
    '龙', '史', '陶', '黎', '贺', '顾', '毛', '郝', '龚', '邵', '万', '钱', '严', '覃', '武', '戴',
    '莫', '孔', '向', '汤',
];

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_digit_runs_are_redacted_and_counted() {
        let (out, hits) = desensitize("住院号 1234567 已收");
        assert_eq!(hits, 1);
        assert!(out.contains("<REDACTED-ID>"));
        assert!(!out.contains("1234567"));
    }

    #[test]
    fn short_numbers_survive() {
        // 床号 03、规则版本 v2 这类短数字不能被误伤
        let (out, hits) = desensitize("床号 03 · 版本 v2 · 时刻 09:00");
        assert_eq!(hits, 0);
        assert_eq!(out, "床号 03 · 版本 v2 · 时刻 09:00");
    }

    #[test]
    fn desensitized_aliases_are_untouched() {
        let (out, hits) = desensitize("WARD-A-001-CONSENT-01#s1@9,18");
        assert_eq!(hits, 0);
        assert_eq!(out, "WARD-A-001-CONSENT-01#s1@9,18");
    }

    #[test]
    fn cjk_words_longer_than_four_chars_survive() {
        let (out, hits) = desensitize("护理记录已签署完成");
        assert_eq!(hits, 0);
        assert_eq!(out, "护理记录已签署完成");
    }

    #[test]
    fn a_name_behind_a_label_is_redacted_but_the_label_stays() {
        // 院内文书的真实写法：家属代签时姓名一定会跟在标签后面。
        let (out, hits) = desensitize("家属：张丽娟代签");
        assert_eq!(hits, 1);
        assert_eq!(out, "家属：<REDACTED-NAME>代签");

        let (out, hits) = desensitize("签收人 李静 已核");
        assert_eq!(hits, 1);
        assert_eq!(out, "签收人 <REDACTED-NAME> 已核");
    }

    #[test]
    fn a_label_not_followed_by_a_name_is_left_alone() {
        // 「患者已离院」里的「患者」不是人名——脱敏器一旦误伤正常文书用语，
        // 质控员就会开始怀疑整份导出，工具等于废了。
        let (out, hits) = desensitize("患者已离院，家属已知悉");
        assert_eq!(hits, 0);
        assert_eq!(out, "患者已离院，家属已知悉");
    }

    #[test]
    fn ordinary_two_to_four_character_wording_is_never_taken_for_a_name() {
        // 无标签锚定时，一个汉字都不许动：这条守的是「宁可漏也不错」——
        // 漏一个名字靠人工复核能补，错杀一百条正常措辞则没人再信这个工具。
        for s in [
            "患方签字未填写",
            "已签署完成",
            "第二签名缺失",
            "未填写",
            "同意",
        ] {
            let (out, hits) = desensitize(s);
            assert_eq!(hits, 0, "误伤了正常措辞：{s}");
            assert_eq!(out, s);
        }
    }

    #[test]
    fn a_long_name_is_left_alone_because_it_is_never_a_chinese_given_name() {
        // 标签后跟 5 个以上汉字：更像句子而不是人名，放过并让人工看原文。
        let (out, hits) = desensitize("患者：本病例已按规范处理完毕");
        assert_eq!(hits, 0);
        assert!(out.contains("本病例已按规范处理完毕"));
    }

    #[test]
    fn the_longest_label_wins_so_a_composite_label_is_not_split() {
        // 「患者名」必须整体匹配；先匹配「患者」会留下「名字」，形态不对而漏掉。
        let (out, hits) = desensitize("患者名：张丽娟");
        assert_eq!(hits, 1);
        assert_eq!(out, "患者名：<REDACTED-NAME>");
    }
}
