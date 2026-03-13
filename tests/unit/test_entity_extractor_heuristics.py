"""Tests for new heuristic entity extraction rules added today."""

from memoria.core.memory.graph.entity_extractor import extract_entities_lightweight


def _names(text: str) -> set[str]:
    return {e.display_name for e in extract_entities_lightweight(text)}


def _types(text: str) -> dict[str, str]:
    return {e.display_name: e.entity_type for e in extract_entities_lightweight(text)}


class TestCapitalizedWordExtraction:
    """Capitalized English words in CJK context → tech entities."""

    def test_single_capitalized_word(self):
        # "Spark" alone (sentence start) → extracted
        assert "Spark" in _names("Spark 任务最近频繁报 OutOfMemoryError")

    def test_multi_word_capitalized(self):
        assert "Apache Spark" in _names("ETL pipeline 使用 Apache Spark 处理")

    def test_dotted_name(self):
        assert "Next.js" in _names("新项目前端选用 Next.js 14")
        assert "Node.js" in _names("Next.js 14 要求 Node.js 18.17 以上版本")

    def test_common_english_words_excluded(self):
        names = _names("The server is running on Monday")
        assert "The" not in names
        assert "Monday" not in names

    def test_multi_word_tech_phrase(self):
        assert "React Server Components" in _names(
            "React Server Components 性能测试结果很好"
        )

    def test_single_letter_excluded(self):
        # "S" in "Stripe SDK" should not be extracted as a separate entity
        names = _names("Bob 负责 payment-service 模块，使用 Stripe SDK。")
        assert "S" not in names
        assert "Stripe" in names


class TestAcronymExtraction:
    """Uppercase acronyms: OAuth, DNS, ETL, RSC, API."""

    def test_oauth(self):
        assert "OAuth" in _names("排查发现是 OAuth 回调接口响应慢")

    def test_dns(self):
        assert "DNS" in _names("DNS 切换后，service-mesh 的服务发现开始间歇性失败")

    def test_etl(self):
        assert "ETL" in _names("ETL pipeline 使用 Apache Spark 处理")

    def test_rsc(self):
        assert "RSC" in _names(
            "RSC 在嵌套动态路由场景下有严重的 hydration mismatch bug"
        )

    def test_api(self):
        assert "API" in _names("后端 API 部署在 AWS ECS 上")


class TestChineseNameExtraction:
    """Chinese person names via surname + given char pattern."""

    def test_name_after_de(self):
        # 前端组的张伟 — 的 before name
        assert "张伟" in _names("前端组的张伟提出了一个 React 性能优化方案")

    def test_name_before_zai(self):
        # 张伟在排查 — 在 after name
        assert "张伟" in _names("后端组的张伟在排查 MySQL 慢查询")

    def test_name_before_verb(self):
        # 王芳在调试
        assert "王芳" in _names("王芳在调试 Kubernetes 的 Ingress 配置")

    def test_at_mention_still_works(self):
        # @李明 should still be extracted via @mention rule
        assert "李明" in _names("@李明 是数据平台组的 tech lead")

    def test_common_word_not_name(self):
        # 平均 should not be extracted (平 is a surname but 均 is not a verb/particle)
        assert "平均" not in _names("排查发现是 OAuth 回调接口响应慢，平均 3 秒。")


class TestServiceNameExtraction:
    """Hyphenated service/component names."""

    def test_auth_service(self):
        assert "auth-service" in _names("auth-service 使用了 jsonwebtoken 库")

    def test_payment_service(self):
        assert "payment-service" in _names("Bob 负责 payment-service 模块")

    def test_service_mesh(self):
        assert "service-mesh" in _names(
            "DNS 切换后，service-mesh 的服务发现开始间歇性失败"
        )

    def test_user_service(self):
        assert "user-service" in _names(
            "服务发现失败导致 gateway 到 user-service 的连接超时"
        )

    def test_non_service_hyphen_excluded(self):
        # "well-known" is not a service name
        names = _names("This is a well-known issue")
        assert "well-known" not in names
