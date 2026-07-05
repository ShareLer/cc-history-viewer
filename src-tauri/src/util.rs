/// 取路径末级目录名作为展示名。
pub fn project_name(path: &str) -> String {
    let trimmed = path.trim_end_matches(|c| c == '/' || c == '\\');
    match trimmed.rsplit(|c| c == '/' || c == '\\').next() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => path.to_string(),
    }
}
