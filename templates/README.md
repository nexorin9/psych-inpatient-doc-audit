# templates —— 段落骨架说明

补写预览走的是「**骨架渲染 → 与原文双栏对照**」这条路：把该文书类型的必填栏位
按院内表单顺序排一遍，已填的按原值渲染、只把**真缺的那几行**标成新增，护士长
看到的 diff 里不会一屏全是新增。

**骨架本身不在本目录，而在规则集 `rulesets/hospital.yaml` 的 `templates:` 段**：

```yaml
templates:
  restraint_consent:
    doc-type: "精神科约束知情同意书"
    fields:
      - { key: "约束事由", required: true }
      - { key: "患方意见", required: true }
      - { key: "患方签字", required: true }
```

之所以跟着规则集走而不另立一套模板文件：骨架里的**必填性与判定用的是同一份声明**。
拆成两处的话，改了规则集忘了改模板（或反过来），补写预览就会建议护士长去填一个
规则并不要求的栏位 —— 而真正缺的那一栏反而没出现在预览里。

- 渲染与 diff：`core/crates/psychdoc-model/src/preview.rs`
  （`render_skeleton` / `preview_fixback` / `diff_lines`）
- 栏位定义与必填性：`rulesets/hospital.yaml` 的 `templates:` 段
- 调用入口：CLI `psychdoc writeback-dryrun` 与桌面壳的「预览补写」按钮
  （FFI 入口 `preview_fixback`，见 `docs/cli.md`）

模板内容**不参与诊断判定**：判定只看规则引擎读到的字段值，预览只是把这些值
按表单顺序重新排一遍给人看。